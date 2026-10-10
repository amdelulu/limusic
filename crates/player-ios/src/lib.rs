//! iOS stand-in for the libmpv `player` crate. Same surface, so `state.rs` and friends are
//! untouched; the sound is made by an `<audio>` element in the WebView.
//!
//! Division of labour:
//!
//! * **Commands** go out through a bridge ([`Player::set_bridge`]) as small JSON objects, which the
//!   app emits to the page as the `ios-player` event. Until the page says it is [`WebEvent::Ready`]
//!   they are held, and only the newest `load` is kept: a track the user has since replaced is not
//!   worth starting.
//! * **Events** come back as [`WebEvent`]s through [`Player::on_web_event`] and are translated into
//!   the [`PlayerEvent`]s mpv's event thread would have produced.
//!
//! Every `load` carries a sequence number and the page echoes it, so a late event from a track that
//! has already been replaced (an `ended` or `error` from the old source) is dropped here.
//!
//! What iOS does not do, deliberately: gapless/crossfade (the app falls back to its "load the next
//! track explicitly" path, see `AppState::on_track_ended`), a software volume (iOS fixes
//! `HTMLMediaElement.volume` at 1; the hardware buttons own the level), loudness gain, and pitch.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Same wording as the mpv player: it reaches the user as a toast.
    #[error("Pitch shifting isn't available in this build")]
    NoPitchFilter,
}

/// Mirror of the libmpv crate's enum: `lib.rs::spawn_event_pump` matches on every variant, so the
/// set has to stay identical even where this backend never produces one.
#[derive(Debug, Clone)]
pub enum PlayerEvent {
    Position(f64),
    Duration(f64),
    /// Playback started or stopped, emitted only on a real change.
    Playing(bool),
    /// A pause or resume the listener caused (not a track starting or ending).
    Paused(bool),
    /// One track finished normally; the app advances the queue.
    TrackEnded,
    /// One track died (its URL was refused, the format is unsupported, the network dropped).
    TrackFailed(String),
    AudioDeviceLost,
    LookaheadFailed(String),
    VideoFailed(String),
    Error(String),
}

/// What the page tells us. `seq` echoes the `load` the event belongs to.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WebEvent {
    /// The page has installed its listener and can take commands.
    Ready,
    Position { seq: u64, position: f64 },
    Duration { seq: u64, duration: f64 },
    /// Audio is actually coming out (the element's `playing`).
    Playing { seq: u64 },
    /// The element paused for a reason other than reaching the end.
    Paused { seq: u64 },
    Ended { seq: u64 },
    Error { seq: u64, message: String },
}

type Bridge = Box<dyn Fn(Value) + Send + Sync>;

struct Inner {
    tx: UnboundedSender<PlayerEvent>,
    bridge: Mutex<Option<Bridge>>,
    /// The page can take commands.
    ready: AtomicBool,
    /// Newest `load` issued while the page was not ready.
    pending_load: Mutex<Option<Value>>,
    seq: AtomicU64,
    /// Nothing loaded, or the last track ended or failed. What `AppState::on_track_ended` asks to
    /// tell "advanced gaplessly" from "stalled".
    idle: AtomicBool,
    playing: AtomicBool,
    /// A `Paused(true)` has been sent and no `Paused(false)` has followed.
    paused_announced: AtomicBool,
    volume: AtomicI64,
    path: Mutex<Option<String>>,
}

impl Inner {
    fn send(&self, cmd: Value) {
        if !self.ready.load(Ordering::Acquire) {
            // Only a load is worth replaying once the page is up; the rest describes a moment that
            // will have passed.
            if cmd.get("op").and_then(Value::as_str) == Some("load") {
                *self.pending_load.lock().unwrap() = Some(cmd);
            }
            return;
        }
        self.emit(cmd);
    }

    fn emit(&self, cmd: Value) {
        match self.bridge.lock().unwrap().as_ref() {
            Some(bridge) => bridge(cmd),
            None => tracing::debug!("ios player: no bridge installed, dropping command"),
        }
    }

    fn event(&self, ev: PlayerEvent) {
        let _ = self.tx.send(ev);
    }
}

pub struct Player {
    inner: Arc<Inner>,
    events: Option<UnboundedReceiver<PlayerEvent>>,
}

impl Player {
    pub fn new(_cache_dir: &str) -> Result<Self, Error> {
        let (tx, rx) = unbounded_channel();
        let inner = Arc::new(Inner {
            tx,
            bridge: Mutex::new(None),
            ready: AtomicBool::new(false),
            pending_load: Mutex::new(None),
            seq: AtomicU64::new(0),
            idle: AtomicBool::new(true),
            playing: AtomicBool::new(false),
            paused_announced: AtomicBool::new(false),
            volume: AtomicI64::new(100),
            path: Mutex::new(None),
        });
        Ok(Player { inner, events: Some(rx) })
    }

    /// Install the function that delivers a command to the page. The app calls this once at setup
    /// with a closure that emits the `ios-player` event.
    pub fn set_bridge(&self, bridge: impl Fn(Value) + Send + Sync + 'static) {
        *self.inner.bridge.lock().unwrap() = Some(Box::new(bridge));
    }

    pub fn take_events(&mut self) -> Option<UnboundedReceiver<PlayerEvent>> {
        self.events.take()
    }

    /// Feed one report from the page. Returns whether it was accepted (the command that carries it
    /// answers the page, which retries `Ready` until it is).
    pub fn on_web_event(&self, ev: WebEvent) -> bool {
        let i = &self.inner;
        match ev {
            WebEvent::Ready => {
                i.ready.store(true, Ordering::Release);
                // Take it out first: `emit` calls the bridge, and nothing here should hold a lock
                // across that.
                let pending = i.pending_load.lock().unwrap().take();
                if let Some(load) = pending {
                    i.emit(load);
                }
            }
            WebEvent::Position { seq, position } => {
                if self.current(seq) && position.is_finite() && position >= 0.0 {
                    i.event(PlayerEvent::Position(position));
                }
            }
            WebEvent::Duration { seq, duration } => {
                if self.current(seq) && duration.is_finite() && duration > 0.0 {
                    i.event(PlayerEvent::Duration(duration));
                }
            }
            WebEvent::Playing { seq } => {
                if !self.current(seq) {
                    return true;
                }
                if !i.playing.swap(true, Ordering::AcqRel) {
                    i.event(PlayerEvent::Playing(true));
                }
                if i.paused_announced.swap(false, Ordering::AcqRel) {
                    i.event(PlayerEvent::Paused(false));
                }
            }
            WebEvent::Paused { seq } => {
                if !self.current(seq) {
                    return true;
                }
                if i.playing.swap(false, Ordering::AcqRel) {
                    i.event(PlayerEvent::Playing(false));
                }
                if !i.paused_announced.swap(true, Ordering::AcqRel) {
                    i.event(PlayerEvent::Paused(true));
                }
            }
            WebEvent::Ended { seq } => {
                if !self.current(seq) {
                    return true;
                }
                // Before the event: the handler asks `is_idle` straight away.
                i.idle.store(true, Ordering::Release);
                i.playing.store(false, Ordering::Release);
                i.event(PlayerEvent::TrackEnded);
            }
            WebEvent::Error { seq, message } => {
                if !self.current(seq) {
                    return true;
                }
                i.idle.store(true, Ordering::Release);
                i.playing.store(false, Ordering::Release);
                i.event(PlayerEvent::TrackFailed(message));
            }
        }
        true
    }

    fn current(&self, seq: u64) -> bool {
        seq == self.inner.seq.load(Ordering::Acquire)
    }

    // ---- the mpv player's surface ------------------------------------------------------------

    /// Load and play a fresh URL. `headers` and `gain_db` are not usable here: a WebView cannot
    /// set request headers (the app's loopback audio proxy adds them upstream), and iOS has no
    /// software volume to apply a gain to.
    pub fn load(
        &self,
        url: &str,
        _headers: &HashMap<String, String>,
        _gain_db: Option<f64>,
        start: Option<f64>,
    ) -> Result<(), Error> {
        let i = &self.inner;
        let seq = i.seq.fetch_add(1, Ordering::AcqRel) + 1;
        i.idle.store(false, Ordering::Release);
        i.playing.store(false, Ordering::Release);
        // A new track is not a pause the listener asked for.
        i.paused_announced.store(false, Ordering::Release);
        *i.path.lock().unwrap() = Some(url.to_owned());
        i.send(json!({ "op": "load", "seq": seq, "url": url, "start": start }));
        Ok(())
    }

    /// No lookahead on iOS: the app then loads the next track itself when this one ends, which is
    /// a short gap instead of a gapless join.
    pub fn enqueue(&self, _url: &str) -> Result<(), Error> {
        Ok(())
    }

    pub fn set_crossfade(&self, _secs: Option<f64>) {}

    pub fn clear_playlist(&self) -> Result<(), Error> {
        Ok(())
    }

    pub fn stop(&self) -> Result<(), Error> {
        let i = &self.inner;
        // Invalidates whatever the page still reports for the track being dropped.
        i.seq.fetch_add(1, Ordering::AcqRel);
        i.idle.store(true, Ordering::Release);
        i.playing.store(false, Ordering::Release);
        *i.path.lock().unwrap() = None;
        i.send(json!({ "op": "stop" }));
        Ok(())
    }

    pub fn is_idle(&self) -> bool {
        self.inner.idle.load(Ordering::Acquire)
    }

    pub fn play(&self) -> Result<(), Error> {
        self.inner.send(json!({ "op": "play" }));
        Ok(())
    }

    pub fn pause(&self) -> Result<(), Error> {
        self.inner.send(json!({ "op": "pause" }));
        Ok(())
    }

    pub fn toggle(&self) -> Result<(), Error> {
        if self.inner.playing.load(Ordering::Acquire) {
            self.pause()
        } else {
            self.play()
        }
    }

    pub fn set_loop_file(&self, on: bool) -> Result<(), Error> {
        self.inner.send(json!({ "op": "loop", "on": on }));
        Ok(())
    }

    pub fn seek(&self, position_secs: f64) -> Result<(), Error> {
        if position_secs.is_finite() {
            self.inner.send(json!({ "op": "seek", "position": position_secs.max(0.0) }));
        }
        Ok(())
    }

    /// Remembered so the settings slider reads back what it set; it cannot change the loudness, see
    /// the module docs.
    pub fn set_volume(&self, volume: i64) -> Result<(), Error> {
        self.inner.volume.store(volume, Ordering::Relaxed);
        Ok(())
    }

    pub fn volume(&self) -> i64 {
        self.inner.volume.load(Ordering::Relaxed)
    }

    pub fn set_http_proxy(&self, _proxy: Option<&str>) -> Result<(), Error> {
        Ok(())
    }

    pub fn set_gain(&self, _gain_db: Option<f64>) -> Result<(), Error> {
        Ok(())
    }

    pub fn set_speed(&self, speed: f64) -> Result<(), Error> {
        self.inner.send(json!({ "op": "rate", "value": speed.clamp(0.25, 2.0) }));
        Ok(())
    }

    /// Zero is "no shift" and always fine; anything else needs a DSP the WebView does not have.
    pub fn set_pitch(&self, semitones: i32) -> Result<(), Error> {
        if semitones == 0 {
            Ok(())
        } else {
            Err(Error::NoPitchFilter)
        }
    }

    // ---- music video: not on iOS ---------------------------------------------------------------

    pub fn set_video_for(&self, _audio_url: &str, _video_url: &str) {}

    /// The URL last handed to [`Player::load`], which is what the app compares a video's audio
    /// against.
    pub fn current_path(&self) -> Option<String> {
        self.inner.path.lock().unwrap().clone()
    }

    pub fn set_video_visible(&self, _on: bool) {}

    pub fn set_video_window(&self, _wid: i64) -> Result<(), Error> {
        Ok(())
    }

    pub fn video_output_up(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn player() -> (Player, UnboundedReceiver<PlayerEvent>, Arc<StdMutex<Vec<Value>>>) {
        let mut p = Player::new("").unwrap();
        let sent = Arc::new(StdMutex::new(Vec::new()));
        let s = sent.clone();
        p.set_bridge(move |v| s.lock().unwrap().push(v));
        let rx = p.take_events().unwrap();
        (p, rx, sent)
    }

    fn drain(rx: &mut UnboundedReceiver<PlayerEvent>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(format!("{e:?}"));
        }
        out
    }

    fn load(p: &Player) {
        p.load("http://127.0.0.1:1/t/a/x", &HashMap::new(), None, Some(12.5)).unwrap();
    }

    #[test]
    fn a_load_before_the_page_is_ready_is_held_and_only_the_newest_survives() {
        let (p, _rx, sent) = player();
        load(&p);
        p.load("http://127.0.0.1:1/t/a/y", &HashMap::new(), None, None).unwrap();
        p.play().unwrap(); // dropped: it describes a moment that has passed
        assert!(sent.lock().unwrap().is_empty());
        p.on_web_event(WebEvent::Ready);
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["url"], "http://127.0.0.1:1/t/a/y");
        assert_eq!(sent[0]["seq"], 2);
    }

    #[test]
    fn load_carries_the_start_position_and_is_not_idle() {
        let (p, _rx, sent) = player();
        p.on_web_event(WebEvent::Ready);
        assert!(p.is_idle());
        load(&p);
        assert!(!p.is_idle());
        assert_eq!(sent.lock().unwrap()[0]["start"], 12.5);
        assert_eq!(p.current_path().as_deref(), Some("http://127.0.0.1:1/t/a/x"));
    }

    #[test]
    fn playing_and_pausing_report_once_per_change() {
        let (p, mut rx, _s) = player();
        p.on_web_event(WebEvent::Ready);
        load(&p);
        p.on_web_event(WebEvent::Playing { seq: 1 });
        p.on_web_event(WebEvent::Playing { seq: 1 });
        assert_eq!(drain(&mut rx), ["Playing(true)"]);
        p.on_web_event(WebEvent::Paused { seq: 1 });
        p.on_web_event(WebEvent::Paused { seq: 1 });
        assert_eq!(drain(&mut rx), ["Playing(false)", "Paused(true)"]);
        p.on_web_event(WebEvent::Playing { seq: 1 });
        assert_eq!(drain(&mut rx), ["Playing(true)", "Paused(false)"]);
    }

    #[test]
    fn a_new_track_starting_is_not_announced_as_a_resume() {
        let (p, mut rx, _s) = player();
        p.on_web_event(WebEvent::Ready);
        load(&p);
        p.on_web_event(WebEvent::Playing { seq: 1 });
        drain(&mut rx);
        load(&p); // seq 2, while the first was playing
        p.on_web_event(WebEvent::Playing { seq: 2 });
        assert_eq!(drain(&mut rx), ["Playing(true)"]);
    }

    #[test]
    fn the_end_of_a_track_is_idle_before_the_event_is_seen() {
        let (p, mut rx, _s) = player();
        p.on_web_event(WebEvent::Ready);
        load(&p);
        p.on_web_event(WebEvent::Playing { seq: 1 });
        drain(&mut rx);
        p.on_web_event(WebEvent::Ended { seq: 1 });
        assert!(p.is_idle());
        assert_eq!(drain(&mut rx), ["TrackEnded"]);
    }

    #[test]
    fn events_from_a_replaced_track_are_dropped() {
        let (p, mut rx, _s) = player();
        p.on_web_event(WebEvent::Ready);
        load(&p);
        load(&p); // replaces seq 1
        p.on_web_event(WebEvent::Ended { seq: 1 });
        p.on_web_event(WebEvent::Error { seq: 1, message: "late".into() });
        p.on_web_event(WebEvent::Position { seq: 1, position: 3.0 });
        assert!(drain(&mut rx).is_empty());
        assert!(!p.is_idle());
    }

    #[test]
    fn stop_invalidates_what_the_page_still_reports() {
        let (p, mut rx, _s) = player();
        p.on_web_event(WebEvent::Ready);
        load(&p);
        p.stop().unwrap();
        p.on_web_event(WebEvent::Ended { seq: 1 });
        assert!(drain(&mut rx).is_empty());
        assert!(p.is_idle());
        assert_eq!(p.current_path(), None);
    }

    #[test]
    fn a_failed_track_reports_and_goes_idle() {
        let (p, mut rx, _s) = player();
        p.on_web_event(WebEvent::Ready);
        load(&p);
        p.on_web_event(WebEvent::Error { seq: 1, message: "403".into() });
        assert!(p.is_idle());
        assert_eq!(drain(&mut rx), ["TrackFailed(\"403\")"]);
    }

    #[test]
    fn nonsense_positions_and_durations_are_ignored() {
        let (p, mut rx, _s) = player();
        p.on_web_event(WebEvent::Ready);
        load(&p);
        p.on_web_event(WebEvent::Position { seq: 1, position: f64::NAN });
        p.on_web_event(WebEvent::Position { seq: 1, position: -1.0 });
        p.on_web_event(WebEvent::Duration { seq: 1, duration: f64::INFINITY });
        p.on_web_event(WebEvent::Duration { seq: 1, duration: 0.0 });
        assert!(drain(&mut rx).is_empty());
        p.on_web_event(WebEvent::Position { seq: 1, position: 4.5 });
        p.on_web_event(WebEvent::Duration { seq: 1, duration: 200.0 });
        assert_eq!(drain(&mut rx), ["Position(4.5)", "Duration(200.0)"]);
    }

    #[test]
    fn toggle_follows_what_is_playing() {
        let (p, _rx, sent) = player();
        p.on_web_event(WebEvent::Ready);
        load(&p);
        p.toggle().unwrap();
        p.on_web_event(WebEvent::Playing { seq: 1 });
        p.toggle().unwrap();
        let ops: Vec<String> =
            sent.lock().unwrap().iter().map(|v| v["op"].as_str().unwrap().to_owned()).collect();
        assert_eq!(ops, ["load", "play", "pause"]);
    }

    #[test]
    fn pitch_zero_is_fine_and_anything_else_is_refused() {
        let (p, _rx, _s) = player();
        assert!(p.set_pitch(0).is_ok());
        assert!(matches!(p.set_pitch(3), Err(Error::NoPitchFilter)));
    }

    #[test]
    fn web_events_parse_the_way_the_page_sends_them() {
        let e: WebEvent = serde_json::from_str(r#"{"type":"position","seq":3,"position":1.5}"#).unwrap();
        assert!(matches!(e, WebEvent::Position { seq: 3, .. }));
        let e: WebEvent = serde_json::from_str(r#"{"type":"ready"}"#).unwrap();
        assert!(matches!(e, WebEvent::Ready));
        let e: WebEvent =
            serde_json::from_str(r#"{"type":"error","seq":1,"message":"x"}"#).unwrap();
        assert!(matches!(e, WebEvent::Error { .. }));
    }
}
