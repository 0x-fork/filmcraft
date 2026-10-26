use super::*;

impl FilmcraftApp {
    // ---------------------------------------------------------------- playback

    pub fn toggle_play(&mut self, speed: f64) {
        if self.playback.playing {
            self.stop();
        } else {
            self.play(speed);
        }
    }

    pub fn play(&mut self, speed: f64) {
        self.stop_source();
        if self.session.active_sequence().is_none() {
            return;
        }
        // restart from the end → from the start (Settings ▸ Timeline ▸ "At playback end, return to
        // beginning when restarting playback")
        let dur = self.session.active_sequence().map(|q| q.duration()).unwrap_or_default();
        // (not while a voice-over take starts: it was put exactly where the take must begin, e.g. on empty timeline past the end)
        if speed > 0.0
            && self.session.prefs.timeline.return_to_beginning
            && !self.session.voiceover.recording()
            && self.session.playhead() >= dur - self.session.sequence_rate().frame_duration()
        {
            self.session.set_playhead(Tick::ZERO);
        }
        // A loop restart keeps counting into the same meter.
        if !self.playback.playing || self.playback.speed != speed {
            self.playback.meter.start(speed);
        }
        self.playback.playing = true;
        self.playback.speed = speed;
        self.playback.anchor_tick = self.session.playhead();
        self.playback.anchor_time = -1.0; // set when the preroll ends
        self.playback.preroll = Some(-1.0);
        self.playback.preroll_ready = false;
        if let Some(a) = self.audio.as_mut() {
            a.stop();
        }
        self.playback.audio_clock = false;
        self.playback.audio_retry_at = 0.0;
    }

    /// Start the clock (and audio) once the first frames are ready or the preroll timed out.
    fn end_preroll(&mut self, now: f64) {
        self.playback.preroll = None;
        self.playback.anchor_time = now;
        self.playback.anchor_tick = self.session.playhead();
        self.playback.audio_seen = (0, now);
        self.start_audio(now);
        // Audio Track Mixer: an automation pass runs while playing forward in real time
        if (self.playback.speed - 1.0).abs() < 1e-9 && !self.session.mixrec.active() {
            let t = self.session.playhead();
            let _ = self.session.execute("mixer.recordStart", json!({"time": t.0}));
        }
        // Multi-Camera view: playing records live cuts (keys 1–9 / clicking angles)
        panels::multicam::on_play(self);
        // voice-over: the capture starts with the audio clock
        panels::voiceover::on_play(self);
    }

    /// Play the last Text to Speech preview (`Session::tts_preview`) on the audio output, stopping
    /// timeline playback first. Starting playback again takes the output back.
    pub fn play_tts_preview(&mut self) -> Result<f64, String> {
        let audio = self.session.tts_preview.clone().ok_or("nothing to preview")?;
        if self.playback.playing {
            self.stop();
        }
        let a = self.audio.as_mut().ok_or("no audio output (check Settings > Audio Hardware)")?;
        let sr = a.sample_rate().max(1);
        a.stop();
        let step = f64::from(audio.sample_rate.max(1)) / f64::from(sr);
        let seconds = audio.seconds();
        let mut pos = 0.0f64;
        let fill = Box::new(move |buf: &mut [f32], ch: usize| {
            for frame in buf.chunks_mut(ch.max(1)) {
                let i = pos as usize;
                let f = (pos - i as f64) as f32;
                let x0 = audio.samples.get(i).copied().unwrap_or(0.0);
                let x1 = audio.samples.get(i + 1).copied().unwrap_or(0.0);
                frame.fill(x0 + (x1 - x0) * f);
                pos += step;
            }
        });
        a.start(fill)?;
        self.playback.audio_clock = false;
        Ok(seconds)
    }

    pub fn stop(&mut self) {
        self.playback.playing = false;
        self.playback.stop_at = None;
        self.playback.rest_at = None;
        self.playback.preroll = None;
        self.playback.meter.finish();
        self.frames.stop_prefetch();
        if let Some(a) = self.audio.as_mut() {
            a.stop();
        }
        self.scrub.device_stopped();
        self.playback.audio_clock = false;
        self.playback.audio_retry_at = 0.0;
        if self.session.mixrec.active() {
            let t = self.session.playhead();
            if let Err(e) = self.session.execute("mixer.recordStop", json!({"time": t.0})) {
                self.ui.status = e.to_string();
            }
        }
        panels::multicam::on_stop(self);
        panels::voiceover::on_stop(self);
    }

    pub(super) fn start_audio(&mut self, now: f64) {
        let Some(seq_id) = self.session.state.active_sequence else { return };
        let start_tick = self.session.playhead();
        let Some(a) = self.audio.as_mut() else { return };
        let sr = a.sample_rate();
        // Output frames still follow the accelerated timeline. The pitch preference selects
        // either the original rate conversion or pitch-corrected program audio.
        let Some((mix_rate, mix)) = shuttle::mix(&self.session, seq_id, sr, self.playback.speed) else {
            a.stop();
            self.playback.audio_retry_at = 0.0;
            return;
        };
        let cursor = start_tick.to_units_floor(i64::from(mix_rate));
        // the old stream stops before the new mixer resets the underrun counters
        a.stop();
        // Desktop: mix ahead on a thread so the device callback never waits on decoding.
        #[cfg(not(target_arch = "wasm32"))]
        let fill = play_ahead::spawn(mix, cursor, sr, a.channels(), self.playback.audio_stats.clone());
        #[cfg(target_arch = "wasm32")]
        let fill = {
            let (mut mix, mut cursor) = (mix, cursor);
            Box::new(move |buf: &mut [f32], ch: usize| {
                mix(cursor, buf, ch);
                cursor += (buf.len() / ch.max(1)) as i64;
            })
        };
        match a.start(fill) {
            Ok(_) => {
                self.playback.audio_clock = true;
                self.playback.audio_retry_at = 0.0;
                if let Some(note) = self.audio.as_ref().and_then(|audio| audio.note()) {
                    self.ui.status = note;
                }
            }
            Err(e) => {
                log::warn!("audio output unavailable: {e}");
                self.playback.audio_clock = false;
                self.playback.audio_retry_at = now + AUDIO_RETRY_S;
            }
        }
    }

    /// Advance the Program playhead only while playing; owns preroll completion, audio-clock anchoring, output retry and the fallback to wall time when audio
    /// is lost or stalls.
    pub(super) fn advance_playback(&mut self, ctx: &egui::Context) {
        if !self.playback.playing {
            return;
        }
        let now = self.now();
        if let Some(since) = self.playback.preroll {
            let since = if since < 0.0 { now } else { since };
            self.playback.preroll = Some(since);
            if self.playback.preroll_ready || now - since >= frames::PREROLL_TIMEOUT_S {
                self.end_preroll(now);
            } else {
                ctx.request_repaint();
                return;
            }
        }
        if self.playback.anchor_time < 0.0 {
            self.playback.anchor_time = now;
        }
        if !self.playback.audio_clock && self.playback.audio_retry_at > 0.0 && now >= self.playback.audio_retry_at {
            // The output failed mid-play: open it again where the wall clock got to (what
            // pause/play does by hand), so a recovering device brings the sound back.
            self.playback.audio_retry_at = now + AUDIO_RETRY_S;
            self.playback.anchor_tick = self.session.playhead();
            self.playback.anchor_time = now;
            self.playback.audio_seen = (0, now);
            self.start_audio(now);
        }
        let rate = self.session.sequence_rate();
        if self.playback.audio_clock
            && let Some(audio) = self.audio.as_mut()
        {
            audio.recover();
        }
        let reading = if self.playback.audio_clock { self.audio.as_ref().and_then(|a| a.played_frames().map(|f| (f, a.sample_rate()))) } else { None };
        if self.playback.audio_clock && reading.is_none() {
            // A device error relinquishes its clock immediately. Resume from the displayed
            // frame, rather than jumping to the old wall-clock anchor after buffered playback.
            self.playback.anchor_tick = self.session.playhead();
            self.playback.anchor_time = now;
            self.playback.audio_clock = false;
            self.playback.audio_retry_at = now + AUDIO_RETRY_S;
            if let Some(audio) = self.audio.as_mut() {
                audio.stop();
            }
            log::warn!("audio output lost its playback clock; playing without sound");
            self.ui.status = tl!("Audio output failed: playing without sound (check Settings > Audio Hardware)").into();
        }
        if let Some((f, sr)) = reading {
            if f != self.playback.audio_seen.0 {
                self.playback.audio_seen = (f, now);
            } else if now - self.playback.audio_seen.1 >= AUDIO_STALL_S {
                // the device stopped consuming samples: continue from where the audio got to on
                // the wall clock, without sound
                let played = if sr > 0 { f as f64 / sr as f64 } else { 0.0 };
                self.playback.anchor_tick += Tick::from_seconds_f64(played * self.playback.speed);
                self.playback.anchor_time = now;
                self.playback.audio_clock = false;
                if let Some(a) = self.audio.as_mut() {
                    a.stop();
                }
                log::warn!("audio output stalled (no samples consumed for {AUDIO_STALL_S} s); playing without sound");
                self.ui.status = tl!("Audio output is not responding: playing without sound (check Settings > Audio Hardware)").into();
            }
        }
        let elapsed = match reading {
            Some((f, sr)) if self.playback.audio_clock && sr > 0 => f as f64 / sr as f64,
            _ => now - self.playback.anchor_time,
        };
        let t = self.playback.anchor_tick + Tick::from_seconds_f64(elapsed * self.playback.speed);
        let seq = self.session.active_sequence();
        let dur = seq.map(|q| q.duration()).unwrap_or_default();
        let (lo, hi) = if self.playback.looping {
            (seq.and_then(|q| q.mark_in).unwrap_or(Tick::ZERO), seq.and_then(|q| q.mark_out).map(|o| o + rate.frame_duration()).unwrap_or(dur))
        } else {
            (Tick::ZERO, dur)
        };
        if let Some(end) = self.playback.stop_at.filter(|e| t >= *e && self.playback.speed > 0.0 && !self.playback.looping) {
            self.session.set_playhead(self.playback.rest_at.unwrap_or(end));
            self.stop();
        } else if t >= hi && self.playback.speed > 0.0 && (self.playback.looping || !self.session.voiceover.recording()) {
            // a voice-over take keeps playing (and recording) into the empty timeline past the end
            if self.playback.looping {
                self.session.set_playhead(lo);
                self.play(self.playback.speed);
            } else {
                self.session.set_playhead(hi - rate.frame_duration());
                self.stop();
            }
        } else if t <= Tick::ZERO && self.playback.speed < 0.0 {
            self.session.set_playhead(Tick::ZERO);
            self.stop();
        } else {
            self.session.set_playhead(t);
        }
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod clock_tests {
    use super::*;

    /// The egui clock starts at the integration's beginning, which can precede `clock_start`.
    /// Applying the captured offset to host time keeps hidden logic moving instead of freezing at
    /// the last UI time and then jumping when the next UI pass arrives.
    #[test]
    fn now_applies_the_ui_clock_offset_and_keeps_the_test_override_first() {
        let mut app = FilmcraftApp::new(Session::default());
        app.ui_clock = 123.0;
        app.ui_clock_offset = Some(1_000.0);
        assert!(app.now() >= 1_000.0, "the captured offset moves the host clock to egui time");
        app.ui_clock = 2_000.0;
        assert_eq!(app.now(), 2_000.0, "an egui clock ahead of host+offset still wins");
        app.set_now_for_test(7.0);
        assert_eq!(app.now(), 7.0, "the deterministic test override keeps precedence");
    }
}

#[cfg(test)]
mod audio_recovery_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    struct Device {
        ready: bool,
        starts: Arc<AtomicUsize>,
        reading: Option<u64>,
    }

    impl AudioOut for Device {
        fn sample_rate(&self) -> u32 {
            48000
        }
        fn channels(&self) -> usize {
            2
        }
        fn configure(&mut self, hardware: &filmcraft_engine::settings::AudioHardwarePrefs, _: Option<u32>) {
            self.ready = hardware.default_output == "available";
        }
        fn start(&mut self, _: Box<dyn FnMut(&mut [f32], usize) + Send>) -> Result<u32, String> {
            self.starts.fetch_add(1, Ordering::Relaxed);
            if self.ready { Ok(48000) } else { Err("device unavailable".into()) }
        }
        fn stop(&mut self) {}
        fn played_frames(&self) -> Option<u64> {
            self.reading
        }
    }

    #[test]
    fn forward_shuttle_playback_keeps_audio_enabled() {
        assert_eq!(shuttle_mix_rate(48_000, 1.0), Some(48_000));
        assert_eq!(shuttle_mix_rate(48_000, 2.0), Some(24_000));
        assert_eq!(shuttle_mix_rate(48_000, 4.0), Some(12_000));
        assert_eq!(shuttle_mix_rate(48_000, -1.0), None);
        assert_eq!(shuttle_mix_rate(0, 2.0), None);
        assert_eq!(shuttle_mix_rate(48_000, f64::NAN), None);

        let mut session = Session::default();
        session.execute("file.newSequence", json!({"width":16,"height":16})).unwrap();
        let mut app = FilmcraftApp::new(session);
        let starts = Arc::new(AtomicUsize::new(0));
        app.audio = Some(Box::new(Device { ready: true, starts: starts.clone(), reading: Some(0) }));
        app.play(2.0);
        app.end_preroll(0.0);
        assert_eq!(starts.load(Ordering::Relaxed), 1, "2x playback must start audio output");
        assert!(app.playback.audio_clock, "the device is still the master clock");
        app.stop();
    }

    /// Capture the block primed by the real shuttle playback path, without relying on a
    /// physical device or consuming the producer faster than a device would.
    #[test]
    fn maintain_pitch_shuttling_controls_the_samples_sent_to_the_device() {
        use std::sync::Mutex;
        struct Capture(Arc<Mutex<Vec<f32>>>);
        impl AudioOut for Capture {
            fn start(&mut self, mut fill: Box<dyn FnMut(&mut [f32], usize) + Send>) -> Result<u32, String> {
                let mut buf = vec![0.0; 2048];
                fill(&mut buf, 2);
                *self.0.lock().unwrap() = buf;
                Ok(48_000)
            }
            fn stop(&mut self) {}
            fn sample_rate(&self) -> u32 {
                48_000
            }
            fn played_frames(&self) -> Option<u64> {
                Some(4_800)
            }
        }
        fn assert_shuttle_tones(buf: &[f32], maintain: bool, speed: f64) {
            let amplitude = |samples: &[f32], ch: usize, hz: f64| {
                let (mut re, mut im) = (0.0, 0.0);
                for (i, frame) in samples.as_chunks::<2>().0.iter().enumerate() {
                    let phase = std::f64::consts::TAU * hz * i as f64 / 48_000.0;
                    re += f64::from(frame[ch]) * phase.cos();
                    im += f64::from(frame[ch]) * phase.sin();
                }
                2.0 * re.hypot(im) / (samples.len() / 2) as f64
            };
            for (ch, tone) in [440.0, 660.0].into_iter().enumerate() {
                let hz = if maintain { tone } else { tone * speed };
                let level = amplitude(buf, ch, hz);
                assert!(level > 0.08, "maintain={maintain}, speed={speed}, channel={ch}: {hz} Hz level={level}");
                if speed != 1.0 {
                    let wrong = if maintain { tone * speed } else { tone };
                    assert!(amplitude(buf, ch, wrong) < level * 0.3, "wrong pitch {wrong} Hz at {speed}x, maintain={maintain}");
                }
            }
        }
        let mut session = Session::default();
        session.execute("file.newSequence", json!({"width":16,"height":16})).unwrap();
        let inter: Vec<f32> =
            (0..48_000 * 8).flat_map(|i| [440.0, 660.0].map(|hz| (0.2 * (std::f64::consts::TAU * hz * i as f64 / 48_000.0).sin()) as f32)).collect();
        let bytes: Arc<[u8]> = filmcraft_engine::previews::write_wav_f32(&inter, 48_000).into();
        let item = filmcraft_engine::commands::import_bytes(&mut session, "/shuttle-tone.wav", bytes, None).unwrap();
        session.execute("timeline.place", json!({"item":item.0,"audioTrack":"A1","seconds":0.0})).unwrap();
        let mut app = FilmcraftApp::new(session);
        let samples = Arc::new(Mutex::new(Vec::new()));
        app.audio = Some(Box::new(Capture(samples.clone())));
        let mut normal = None;
        for maintain in [true, false] {
            app.session.prefs.audio.maintain_pitch_shuttling = maintain;
            for speed in [0.5, 1.0, 1.5, 2.0, 4.0] {
                app.session.set_playhead(Tick::from_seconds_f64(1.0));
                let expected = app.session.sequence_rate().snap(app.session.playhead() + Tick::from_seconds_f64(0.1 * speed));
                app.play(speed);
                app.end_preroll(0.0);
                assert!(app.playback.audio_clock);
                let buf = samples.lock().unwrap();
                assert_shuttle_tones(&buf, maintain, speed);
                if speed == 1.0 {
                    if let Some(previous) = &normal {
                        assert_eq!(&*buf, previous, "1x must bypass pitch processing");
                    } else {
                        normal = Some(buf.clone());
                    }
                }
                drop(buf);
                app.advance_playback(&egui::Context::default());
                assert_eq!(app.session.playhead(), expected, "the device clock must advance the timeline at {speed}x, snapped to a video frame");
                app.stop();
            }
        }
    }

    #[test]
    fn changing_pitch_preference_restarts_audio_at_the_current_playhead() {
        let mut session = Session::default();
        session.execute("file.newSequence", json!({"width":16,"height":16})).unwrap();
        let ctx = egui::Context::default();
        let mut app = FilmcraftApp::new(session);
        let starts = Arc::new(AtomicUsize::new(0));
        app.audio = Some(Box::new(Device { ready: true, starts: starts.clone(), reading: Some(0) }));
        app.session.prefs.audio_hardware.default_output = "available".into();
        app.apply_prefs(&ctx);
        app.play(2.0);
        app.end_preroll(0.0);
        assert_eq!(starts.load(Ordering::Relaxed), 1);
        app.session.set_playhead(Tick::from_seconds_f64(2.0));
        let position = app.session.playhead();
        for (maintain, expected_starts) in [(false, 2), (true, 3)] {
            app.session.execute("prefs.set", json!({"key":"audio.maintainPitchShuttling","value":maintain})).unwrap();
            app.apply_prefs(&ctx);
            assert_eq!(starts.load(Ordering::Relaxed), expected_starts);
            assert!(app.playback.audio_clock);
            assert_eq!(app.playback.speed, 2.0);
            assert_eq!(app.playback.anchor_tick, app.session.playhead());
            assert_eq!(app.session.playhead(), position);
            app.apply_prefs(&ctx);
            assert_eq!(starts.load(Ordering::Relaxed), expected_starts, "unchanged settings do not restart");
        }
        for speed in [-1.0, 8.0] {
            app.play(speed);
            app.end_preroll(0.0);
            assert!(!app.playback.audio_clock, "unsupported speed {speed} remains silent");
        }
        assert_eq!(starts.load(Ordering::Relaxed), 3);
        app.stop();
    }

    #[test]
    fn changing_hardware_recovers_wall_clock_playback_without_rewinding() {
        let mut session = Session::default();
        session.execute("file.newSequence", json!({"width":16,"height":16})).unwrap();
        let ctx = egui::Context::default();
        let mut app = FilmcraftApp::new(session);
        let starts = Arc::new(AtomicUsize::new(0));
        app.audio = Some(Box::new(Device { ready: false, starts: starts.clone(), reading: Some(0) }));
        app.apply_prefs(&ctx);
        app.play(1.0);
        app.end_preroll(0.0);
        assert!(!app.playback.audio_clock);
        app.session.set_playhead(Tick::from_seconds_f64(2.0));
        app.session.prefs.audio_hardware.default_output = "available".into();
        app.apply_prefs(&ctx);
        assert_eq!(starts.load(Ordering::Relaxed), 2);
        assert!(app.playback.audio_clock);
        assert_eq!(app.playback.anchor_tick, app.session.playhead());
        app.stop();
    }

    /// A device that loses its playback clock resumes from the displayed frame and reports a status message.
    #[test]
    fn a_failed_device_clock_resumes_from_the_displayed_frame() {
        let mut session = Session::default();
        session.execute("file.openDemoProject", json!({})).unwrap();
        let ctx = egui::Context::default();
        let mut app = FilmcraftApp::new(session);
        app.audio = Some(Box::new(Device { ready: true, starts: Arc::new(AtomicUsize::new(0)), reading: None }));
        let displayed = app.session.sequence_rate().tick_of(48);
        app.session.set_playhead(displayed);
        app.playback.playing = true;
        app.playback.audio_clock = true;
        app.playback.anchor_tick = Tick::ZERO;
        app.playback.anchor_time = 0.0;
        app.set_now_for_test(10.0);
        let mut output = ctx.run_ui(egui::RawInput { time: Some(10.0), ..Default::default() }, |ui| app.advance_playback(ui.ctx()));
        output.textures_delta.clear();
        assert!(!app.playback.audio_clock);
        assert_eq!(app.playback.anchor_tick, displayed);
        assert_eq!(app.session.playhead(), displayed);
        assert!(app.ui.status.contains("Audio output failed"));
        app.stop();
    }

    /// A stream the device dropped, which `recover` restarts on the same clock.
    struct Restartable {
        reading: Option<u64>,
    }

    impl AudioOut for Restartable {
        fn start(&mut self, _: Box<dyn FnMut(&mut [f32], usize) + Send>) -> Result<u32, String> {
            Ok(48000)
        }
        fn stop(&mut self) {}
        fn sample_rate(&self) -> u32 {
            48000
        }
        fn played_frames(&self) -> Option<u64> {
            self.reading
        }
        fn recover(&mut self) {
            self.reading = self.reading.or(Some(48_000));
        }
    }

    /// A recovered stream keeps the device clock and the sound keeps playing.
    #[test]
    fn a_restarted_stream_keeps_the_device_clock_and_the_sound() {
        let mut session = Session::default();
        session.execute("file.openDemoProject", json!({})).unwrap();
        let ctx = egui::Context::default();
        let mut app = FilmcraftApp::new(session);
        app.audio = Some(Box::new(Restartable { reading: None }));
        app.playback.playing = true;
        app.playback.audio_clock = true;
        app.playback.anchor_tick = Tick::ZERO;
        app.playback.anchor_time = 0.0;
        app.playback.audio_seen = (0, 10.0);
        app.set_now_for_test(10.0);
        let mut output = ctx.run_ui(egui::RawInput { time: Some(10.0), ..Default::default() }, |ui| app.advance_playback(ui.ctx()));
        output.textures_delta.clear();
        assert!(app.playback.audio_clock, "the restarted stream is still the master clock");
        assert!(!app.ui.status.contains("Audio output failed"));
        // 48 000 frames played at 48 kHz: one second in, on a frame boundary
        let (shown, second) = (app.session.playhead(), Tick::from_seconds_f64(1.0));
        assert!(
            shown <= second && shown + app.session.sequence_rate().frame_duration() > second,
            "the playhead follows the frames the device played: {shown:?}"
        );
        app.stop();
    }

    /// A device whose stream dies mid-play (`ticking`) or whose open fails (`opens`).
    struct Reviving {
        starts: Arc<AtomicUsize>,
        opens: Arc<AtomicBool>,
        ticking: Arc<AtomicBool>,
    }

    impl AudioOut for Reviving {
        fn start(&mut self, _: Box<dyn FnMut(&mut [f32], usize) + Send>) -> Result<u32, String> {
            self.starts.fetch_add(1, Ordering::Relaxed);
            if self.opens.load(Ordering::Relaxed) { Ok(48_000) } else { Err("device unavailable".into()) }
        }
        fn stop(&mut self) {}
        fn sample_rate(&self) -> u32 {
            48_000
        }
        fn played_frames(&self) -> Option<u64> {
            self.ticking.load(Ordering::Relaxed).then_some(0)
        }
    }

    /// A lost output is reopened on retry and the sound returns without pause/play.
    #[test]
    fn a_lost_output_is_reopened_and_the_sound_returns() {
        let mut session = Session::default();
        session.execute("file.openDemoProject", json!({})).unwrap();
        let ctx = egui::Context::default();
        let mut app = FilmcraftApp::new(session);
        let starts = Arc::new(AtomicUsize::new(0));
        let ticking = Arc::new(AtomicBool::new(true));
        app.audio = Some(Box::new(Reviving { starts: starts.clone(), opens: Arc::new(AtomicBool::new(true)), ticking: ticking.clone() }));
        app.play(1.0);
        app.end_preroll(0.0);
        assert!(app.playback.audio_clock && starts.load(Ordering::Relaxed) == 1);
        // the stream dies: the device stops reporting a playback clock
        ticking.store(false, Ordering::Relaxed);
        app.set_now_for_test(10.0);
        let mut output = ctx.run_ui(egui::RawInput { time: Some(10.0), ..Default::default() }, |ui| app.advance_playback(ui.ctx()));
        output.textures_delta.clear();
        assert!(!app.playback.audio_clock);
        // the device recovers before the next retry: playback reopens it itself
        ticking.store(true, Ordering::Relaxed);
        app.set_now_for_test(10.0 + AUDIO_RETRY_S + 0.5);
        let mut output = ctx.run_ui(egui::RawInput { time: Some(10.0 + AUDIO_RETRY_S + 0.5), ..Default::default() }, |ui| app.advance_playback(ui.ctx()));
        output.textures_delta.clear();
        assert_eq!(starts.load(Ordering::Relaxed), 2, "the output is reopened without pause/play");
        assert!(app.playback.audio_clock, "the fresh stream is the master clock again");
        app.stop();
    }

    /// A device that becomes available mid-playback starts sounding on the next retry.
    #[test]
    fn a_device_that_appears_mid_playback_starts_sounding() {
        let mut session = Session::default();
        session.execute("file.openDemoProject", json!({})).unwrap();
        let ctx = egui::Context::default();
        let mut app = FilmcraftApp::new(session);
        let starts = Arc::new(AtomicUsize::new(0));
        let opens = Arc::new(AtomicBool::new(false));
        app.audio = Some(Box::new(Reviving { starts: starts.clone(), opens: opens.clone(), ticking: Arc::new(AtomicBool::new(true)) }));
        app.play(1.0);
        app.end_preroll(0.0);
        assert!(!app.playback.audio_clock, "no device yet: playing on the wall clock");
        // headphones plug in while playing: the next retry opens them
        opens.store(true, Ordering::Relaxed);
        app.set_now_for_test(AUDIO_RETRY_S + 0.5);
        let mut output = ctx.run_ui(egui::RawInput { time: Some(AUDIO_RETRY_S + 0.5), ..Default::default() }, |ui| app.advance_playback(ui.ctx()));
        output.textures_delta.clear();
        assert_eq!(starts.load(Ordering::Relaxed), 2, "the output is retried without pause/play");
        assert!(app.playback.audio_clock);
        app.stop();
    }
}

#[cfg(test)]
mod voiceover_playback_tests {
    use super::*;

    /// A voice-over take into an empty timeline past the end keeps playing and recording.
    #[test]
    fn a_voice_over_take_records_into_the_empty_timeline_past_the_end() {
        let mut session = Session::default();
        session.execute("file.openDemoProject", json!({})).unwrap();
        session.execute("audio.voiceover.settings", json!({"prerollSeconds": 0.0})).unwrap();
        session.prefs.timeline.return_to_beginning = true;
        let ctx = egui::Context::default();
        let mut app = FilmcraftApp::new(session);
        app.audio = None;
        let dur = app.session.active_sequence().map(|q| q.duration()).unwrap();
        let from = app.session.sequence_rate().snap(dur + Tick::from_seconds_f64(60.0));
        app.session.set_playhead(from);
        panels::voiceover::toggle(&mut app, None).unwrap();
        // play() keeps the record point instead of returning to the beginning
        assert!(app.playback.playing);
        assert_eq!(app.session.playhead(), from);
        app.end_preroll(0.0);
        // and one second later the take is still playing and recording, not stopped at the end
        app.set_now_for_test(1.0);
        let mut output = ctx.run_ui(egui::RawInput { time: Some(1.0), ..Default::default() }, |ui| app.advance_playback(ui.ctx()));
        output.textures_delta.clear();
        assert!(app.playback.playing);
        assert!(app.session.voiceover.recording());
        assert!(app.session.playhead() > from, "{:?} <= {from:?}", app.session.playhead());
        app.session.execute("audio.voiceover.stop", json!({"discard": true})).unwrap();
        app.stop();
    }
}
