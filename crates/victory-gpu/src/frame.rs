//! Frame pacing: the title sets a frame interval and waits for each frame
//! to begin. Ours is a clock started by the first interval set, ticking
//! once per interval; the token is the frame number. Each event kind
//! (origin, vblank, flip) happens once per tick, so two waits for the same
//! kind never end on the same tick: the title polls the origin
//! with "token on time-out" (starting its next frame at once) and is paced
//! by a blocking wait for the flip before each present.
//!
//! shortcut: event offsets are ignored (every event is at the tick); tie
//! the clock to presentation if a title paces by them.
//!
//! `VICTORY_FRAME_INTERVAL_MS=<ms>` ticks at that interval whatever the
//! title sets (16.667: 60 a second; 0: no wait at all, every wait ends at
//! once); unset, the title's interval as before. The loader gives the title
//! the matching frame rate too (`victory_loader::frame_rate`), as the title
//! would otherwise keep to its 30 with a limiter of its own.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::object::{HRESULT, S_FALSE, S_OK, This};

/// What a wait returns when its time is up first.
const TIMEOUT: HRESULT = S_FALSE;
/// The frame-origin event: the one that starts frames and makes tokens.
const ORIGIN: u32 = 0;
/// Make the frame's token even if the wait times out (the title starts the
/// frame early).
const TOKEN_ON_TIMEOUT: u32 = 2;
const SIXTIETH: Duration = Duration::from_micros(16_667);

/// `VICTORY_FRAME_INTERVAL_MS`: the interval asked for in place of the
/// title's (zero: no wait). Read once.
fn asked() -> Option<Duration> {
    static ASKED: std::sync::OnceLock<Option<Duration>> = std::sync::OnceLock::new();
    *ASKED.get_or_init(|| {
        let spec = std::env::var("VICTORY_FRAME_INTERVAL_MS").ok()?;
        let ms = parse_ms(&spec);
        match ms {
            Some(d) => tracing::info!(interval = ?d, "frame interval of ours in place of the title's (VICTORY_FRAME_INTERVAL_MS)"),
            None => tracing::warn!(spec, "VICTORY_FRAME_INTERVAL_MS: expected milliseconds from 0 to 1000; the title's interval kept"),
        }
        ms
    })
}

/// `16.667` → 16,667 us; 0 → zero; negative, more than a second or not a
/// number → `None`.
fn parse_ms(spec: &str) -> Option<Duration> {
    let ms = spec.trim().parse::<f64>().ok().filter(|ms| ms.is_finite() && (0.0..=1000.0).contains(ms))?;
    Some(Duration::from_nanos((ms * 1e6).round() as u64))
}

struct Clock {
    start: Instant,
    interval: Duration,
    /// The tick number at `start` (ticks go on across interval changes).
    first: u64,
}

static CLOCK: Mutex<Option<Clock>> = Mutex::new(None);
static FRAME: AtomicU64 = AtomicU64::new(0);

fn clock() -> std::sync::MutexGuard<'static, Option<Clock>> {
    CLOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Device method: a frame every `length` microseconds. The period is how
/// many intervals a frame may take to reach the display (pipeline depth),
/// not part of the rate.
pub(crate) extern "system" fn set_interval(
    _: This,
    _output: This,
    length: u32,
    _period: u32,
    _flags: u32,
) -> HRESULT {
    let asked_by_title = match length {
        0 => SIXTIETH,
        us => Duration::from_micros(u64::from(us)),
    };
    // Once loading is over with the title at a loading rate of its own
    // (`VICTORY_LOAD_FPS`), what it asks for may still be that rate's
    // (run h26: 66.7 ms again right after, 15 frames a second): the
    // playing interval stays.
    let playing = load_interval().is_some() && victory_core::LOADING_OVER.load(Ordering::Relaxed);
    let interval = asked().unwrap_or(if playing { Duration::from_micros(33_333) } else { asked_by_title });
    tracing::info!(?interval, title = ?asked_by_title, "frame interval");
    // A new interval starts from now, after the last tick handed out.
    let first = LAST
        .iter()
        .map(|t| t.load(Ordering::Relaxed))
        .max()
        .unwrap_or(0);
    *clock() = Some(Clock {
        start: Instant::now(),
        interval,
        first,
    });
    S_OK
}

/// Device method: events are scheduled at every tick.
pub(crate) extern "system" fn schedule(
    _: This,
    _kind: u32,
    _offset: u32,
    _list: *const u8,
    _flags: u32,
) -> HRESULT {
    S_OK
}

/// Frames begun so far (frame waits that ended).
pub(crate) fn current() -> u64 {
    FRAME.load(Ordering::Relaxed)
}

/// For tests of what is done once a frame.
#[cfg(test)]
pub(crate) fn test_advance() {
    FRAME.fetch_add(1, Ordering::Relaxed);
}

/// Per event kind (origin, vblank, flip and later kinds): the last tick a
/// wait for it ended on.
static LAST: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

fn last(kind: u32) -> &'static AtomicU64 {
    &LAST[(kind as usize).min(LAST.len() - 1)]
}

/// The next tick after now and after the last one a wait for `kind` ended
/// on (an event happens once per tick: a wait woken a little early must not
/// let the next one end on the same tick), and how long until it.
fn next_tick(kind: u32) -> (u64, Duration) {
    let mut clock = clock();
    let c = clock.get_or_insert_with(|| Clock {
        start: Instant::now(),
        interval: asked().unwrap_or(SIXTIETH),
        first: 0,
    });
    if c.interval.is_zero() {
        // No wait: every wait ends at once, on a tick of its own.
        let tick = last(kind).load(Ordering::Relaxed) + 1;
        return (tick, Duration::ZERO);
    }
    let elapsed = c.start.elapsed().as_nanos();
    let interval = c.interval.as_nanos().max(1);
    let tick =
        (c.first + (elapsed / interval + 1) as u64).max(last(kind).load(Ordering::Relaxed) + 1);
    let at = u128::from(tick - c.first) * interval;
    (
        tick,
        Duration::from_nanos(at.saturating_sub(elapsed) as u64),
    )
}

/// Milliseconds since our first look at the time ([`loading_gate`]).
fn since_start_ms() -> u64 {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// When a draw of the title's decoration bake was last seen
/// ([`since_start_ms`], plus one; 0: none yet).
static LAST_BAKE: AtomicU64 = AtomicU64::new(0);

/// A draw of the title's skin and decoration bake (`crate::draw`).
pub(crate) fn bake_seen() {
    LAST_BAKE.store(since_start_ms() + 1, Ordering::Relaxed);
}

/// `VICTORY_LOAD_INTERVAL_MS=<ms>[,<hold s>]`: while the title loads, a
/// frame takes at least that long, until no bake was drawn for `hold`
/// seconds (10 unset), or ten minutes in all; then the frame interval as
/// before, for the rest of the run. Off unset or 0.
///
/// The title bakes its characters' skin into textures of its own a set
/// number of frames after it asks for the textures the bake reads; at our
/// 25 frames a second those had not landed yet, and every character's head
/// skin was baked as nothing for the whole run (Linux, RX 7900 XTX; 20 runs
/// of 20, eyes, teeth and hair only). Capped at 5 a second for the whole
/// run (`VICTORY_FRAME_INTERVAL_MS=200`) the skin was there; fxceless's
/// tree, at 3 to 7 a second, had it in some runs and not in others.
///
/// That cap of ours alone (the title kept at its 30) did not bring the
/// skin back; the title's own rate at 5 while it loads does
/// (`VICTORY_LOAD_FPS`, the loader's): the end of loading is found here for
/// that switch too ([`victory_core::LOADING_OVER`]), with `ms` 0.
fn load_interval() -> Option<(u64, u64)> {
    static SPEC: std::sync::OnceLock<Option<(u64, u64)>> = std::sync::OnceLock::new();
    *SPEC.get_or_init(|| {
        let spec = std::env::var("VICTORY_LOAD_INTERVAL_MS").ok();
        let load_fps = victory_core::loading_frame_rate().is_some();
        let mut parts = spec.as_deref().unwrap_or("0").split(',').map(|p| p.trim().parse::<u64>().ok());
        let ms = parts.next().flatten().unwrap_or(0).min(1000);
        let hold = parts.next().flatten().unwrap_or(10);
        if ms == 0 && !load_fps {
            return None;
        }
        tracing::info!(ms, hold, "frames take at least ms while the title loads, until no bake for hold seconds (VICTORY_LOAD_INTERVAL_MS, VICTORY_LOAD_FPS)");
        Some((ms, hold * 1000))
    })
}

/// [`load_interval`]'s wait, at the start of a frame wait for the origin.
fn loading_gate(kind: u32) {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if kind != ORIGIN || DONE.load(Ordering::Relaxed) {
        return;
    }
    let Some((ms, hold)) = load_interval() else {
        DONE.store(true, Ordering::Relaxed);
        return;
    };
    let now = since_start_ms();
    let bake = LAST_BAKE.load(Ordering::Relaxed);
    if (bake != 0 && now > bake - 1 + hold) || now > 600_000 {
        DONE.store(true, Ordering::Relaxed);
        // The title asked for its loading rate's interval and will not ask
        // again: ours from now is the one asked for, or the title's 30.
        let interval = asked().unwrap_or(Duration::from_micros(33_333));
        let first = LAST.iter().map(|t| t.load(Ordering::Relaxed)).max().unwrap_or(0);
        *clock() = Some(Clock {
            start: Instant::now(),
            interval,
            first,
        });
        victory_core::LOADING_OVER.store(true, Ordering::Relaxed);
        tracing::info!(at_ms = now, last_bake_ms = bake.saturating_sub(1), ?interval, "loading over: frames at the playing interval from now (VICTORY_LOAD_INTERVAL_MS, VICTORY_LOAD_FPS)");
        return;
    }
    if ms == 0 {
        return;
    }
    // When the last gated frame began (plus one; 0: none yet).
    static PREV: AtomicU64 = AtomicU64::new(0);
    let prev = PREV.load(Ordering::Relaxed);
    if prev != 0 && now < prev - 1 + ms {
        std::thread::sleep(Duration::from_millis(prev - 1 + ms - now));
    }
    PREV.store(since_start_ms() + 1, Ordering::Relaxed);
}

/// Device method: blocks until the next tick (or the time-out); an origin
/// wait then hands out the new frame's token. Other events keep the token
/// the title passes. Ticks are absolute (start plus whole intervals), so
/// lateness never accumulates.
pub(crate) extern "system" fn wait(
    _: This,
    kind: u32,
    timeout_ms: u32,
    _list: *const u8,
    flags: u32,
    token: *mut u64,
) -> HRESULT {
    let origin = kind == ORIGIN;
    loading_gate(kind);
    let (mut tick, mut wait) = next_tick(kind);
    let limit = Duration::from_millis(u64::from(timeout_ms));
    // A blocking wait for an event that has happened since the last wait
    // for it ended (a frame took longer than a tick) ends at once, on the
    // tick that passed: sleeping on to the next one cost 17 ms of every
    // 730 ms frame (run 58: "2.0 frame waits sleeping 16.8 ms"). Polls (a
    // time-out shorter than a tick) answer as before.
    // `VICTORY_LATE_WAITS=1` sleeps to the next tick, as before.
    static SLEEP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*SLEEP.get_or_init(|| std::env::var_os("VICTORY_LATE_WAITS").is_some_and(|v| v == "1"))
        && limit >= wait
        && tick > last(kind).load(Ordering::Relaxed) + 1
    {
        tick -= 1;
        wait = Duration::ZERO;
    }
    let _called = crate::timeline::span("the title's frame wait");
    let hr = if wait > limit {
        std::thread::sleep(limit);
        TIMEOUT
    } else {
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        last(kind).fetch_max(tick, Ordering::Relaxed);
        S_OK
    };
    #[cfg(all(windows, feature = "diagnostics"))]
    {
        use crate::copystat::{C, add};
        add(C::PaceWaits, 1);
        add(C::PaceUs, wait.min(limit).as_micros() as u64);
    }
    // A frame begun on a time-out takes no tick: its flip still does.
    if origin && (hr == S_OK || flags & TOKEN_ON_TIMEOUT != 0) {
        // Tokens count frames (consecutive), not ticks.
        let frame = FRAME.fetch_add(1, Ordering::Relaxed) + 1;
        pace::begun();
        if !token.is_null() {
            // SAFETY: checked non-null; the title passes its token.
            unsafe { token.write(frame) };
        }
    }
    #[cfg(feature = "diagnostics")]
    tracing::debug!(
        kind,
        timeout_ms,
        flags,
        hr,
        tick,
        wait_us = wait.as_micros() as u64,
        frame = current(),
        "frame wait"
    );
    hr
}

/// How even the frames are (`VICTORY_TIMER_LOG=1`): the times between frame
/// beginnings, logged every 5 s by bands, with the pairs of a short frame
/// next to a long one (a 16/33 alternation shows as many of them).
mod pace {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// Upper edges of the bands, in milliseconds (the last band is open).
    const BANDS: [f64; 6] = [12.0, 20.0, 28.0, 38.0, 55.0, 70.0];

    struct Stats {
        since: Instant,
        last: Option<Instant>,
        last_ms: f64,
        bands: [u32; 7],
        sum_ms: f64,
        max_ms: f64,
        n: u32,
        alternations: u32,
    }

    static STATS: Mutex<Option<Stats>> = Mutex::new(None);

    fn on() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("VICTORY_TIMER_LOG").is_ok_and(|v| v.trim() == "1"))
    }

    pub(super) fn begun() {
        if !on() {
            return;
        }
        let now = Instant::now();
        let mut stats = STATS.lock().unwrap_or_else(|e| e.into_inner());
        let s = stats.get_or_insert_with(|| Stats {
            since: now,
            last: None,
            last_ms: 0.0,
            bands: [0; 7],
            sum_ms: 0.0,
            max_ms: 0.0,
            n: 0,
            alternations: 0,
        });
        if let Some(last) = s.last.replace(now) {
            let ms = (now - last).as_secs_f64() * 1e3;
            let band = BANDS.iter().position(|&edge| ms < edge).unwrap_or(BANDS.len());
            s.bands[band] += 1;
            s.sum_ms += ms;
            s.max_ms = s.max_ms.max(ms);
            if s.n > 0 && (ms < 20.0 && s.last_ms > 28.0 || ms > 28.0 && s.last_ms < 20.0) {
                s.alternations += 1;
            }
            s.last_ms = ms;
            s.n += 1;
        }
        if now - s.since >= Duration::from_secs(5) && s.n > 0 {
            tracing::info!(
                frames = s.n,
                mean_ms = format_args!("{:.2}", s.sum_ms / f64::from(s.n)),
                max_ms = format_args!("{:.1}", s.max_ms),
                bands = ?s.bands,
                alternations = s.alternations,
                "frame pacing: times between frame beginnings (bands <12, <20, <28, <38, <55, <70, more ms)"
            );
            s.since = now;
            s.bands = [0; 7];
            s.sum_ms = 0.0;
            s.max_ms = 0.0;
            s.n = 0;
            s.alternations = 0;
        }
    }
}

/// Device method: no statistics are kept, so none are returned.
pub(crate) extern "system" fn statistics(
    _: This,
    _token: u64,
    _kinds: u32,
    count: *mut u32,
    _out: *mut u8,
) -> HRESULT {
    if !count.is_null() {
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
    }
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_reads_milliseconds() {
        assert_eq!(parse_ms("16.667"), Some(Duration::from_micros(16_667)));
        assert_eq!(parse_ms(" 8.333 "), Some(Duration::from_micros(8_333)));
        assert_eq!(parse_ms("0"), Some(Duration::ZERO));
        assert_eq!(parse_ms("-1"), None);
        assert_eq!(parse_ms("1001"), None);
        assert_eq!(parse_ms("NaN"), None);
        assert_eq!(parse_ms("fast"), None);
    }

    #[test]
    fn waits_end_on_ticks_and_count_frames() {
        let null = std::ptr::null();
        assert_eq!(set_interval(null, null, 2_000, 1, 0), S_OK);
        let mut first = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut first), S_OK);
        let before = Instant::now();
        let mut second = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut second), S_OK);
        assert!(second > first);
        // At the tick, not at the time-out (a busy PC may wake it late).
        assert!(before.elapsed() <= Duration::from_millis(500));
        assert_eq!(wait(null, 0, 0, std::ptr::null(), 0, &mut second), TIMEOUT);
        // Tokens count frames, a frame started early on a time-out too.
        let mut early = 0;
        let flags = TOKEN_ON_TIMEOUT;
        assert_eq!(
            wait(null, 0, 0, std::ptr::null(), flags, &mut early),
            TIMEOUT
        );
        let mut next = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut next), S_OK);
        assert!(early == second + 1 && next == early + 1);
        assert!(LAST[0].load(Ordering::Relaxed) >= 2);

        // The title's loop (the clock is global, so in the same test): an
        // origin poll that times out (and starts the frame), then a
        // blocking flip wait. Each flip takes its own tick.
        const FLIP: u32 = 2;
        // (The ticks are far apart, and it may take a few tries: a busy PC
        // wakes a wait tens of milliseconds late at times.)
        assert_eq!(set_interval(null, null, 60_000, 3, 0), S_OK);
        let mut token = 0;
        let mut four = || -> Vec<u64> {
            (0..4)
                .map(|_| {
                    let _ = wait(null, 0, 0, std::ptr::null(), TOKEN_ON_TIMEOUT, &mut token);
                    assert_eq!(
                        wait(null, FLIP, u32::MAX, std::ptr::null(), 0, &mut token),
                        S_OK
                    );
                    LAST[2].load(Ordering::Relaxed)
                })
                .collect()
        };
        let mut flips = four();
        for _ in 0..10 {
            if flips.windows(2).all(|w| w[1] == w[0] + 1) {
                break;
            }
            flips = four();
        }
        assert!(flips.windows(2).all(|w| w[1] == w[0] + 1), "{flips:?}");
        // Even when a flip wait begins on the tick another just ended on.
        let (tick, _) = next_tick(FLIP);
        assert!(tick > flips[3]);
    }
}
