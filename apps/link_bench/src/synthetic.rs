// SPDX-License-Identifier: AGPL-3.0-or-later

//! Synthetic [`crate::MidiSource`] / [`crate::MidiSink`] impls for the
//! link bench when no real MIDI hardware is attached.
//!
//! TX: emits a hard-coded C-major chord at startup (three NoteOn messages),
//! then idles forever.  This matches the M4 exit-criterion scenario: the
//! receiver should be holding three notes when TX power is killed; the
//! watchdog then forces all-notes-off.
//!
//! RX: logs every received MIDI message and the all-notes-off event via
//! defmt.  When the FeatherWing arrives, swap this for a `BufferedUarte`-
//! backed sink that writes the bytes out to MIDI.

use crate::{MidiSink, MidiSource};
use embassy_time::{Duration, Instant, Timer};

// ── Synthetic source: hold a C major chord ──────────────────────────────────

/// One-shot source that emits NoteOn for C, E, G at boot then idles forever.
pub struct ChordHoldSource {
    sent: u8,
}

impl ChordHoldSource {
    pub const fn new() -> Self {
        Self { sent: 0 }
    }
}

impl Default for ChordHoldSource {
    fn default() -> Self {
        Self::new()
    }
}

/// MIDI status byte for NoteOn on channel 0 = `0x90`.  Velocity 100 is a
/// comfortable mezzo-forte; choose anything 1..=127 (0 would silently
/// turn the note off).
const CHORD_C_MAJOR: &[[u8; 3]] = &[
    [0x90, 60, 100], // C4
    [0x90, 64, 100], // E4
    [0x90, 67, 100], // G4
];

#[derive(Debug, Clone, Copy, defmt::Format)]
pub enum ChordSourceError {}

impl MidiSource for ChordHoldSource {
    type Error = ChordSourceError;

    fn try_next(&mut self, buf: &mut [u8]) -> Result<Option<usize>, Self::Error> {
        if let Some(msg) = CHORD_C_MAJOR.get(self.sent as usize) {
            buf[..3].copy_from_slice(msg);
            self.sent = self.sent.saturating_add(1);
            defmt::info!(
                "synthetic source: NoteOn ch=0 note={} vel={}",
                msg[1],
                msg[2]
            );
            Ok(Some(3))
        } else {
            // Idle forever — the heartbeat timer fills the silence so the
            // receiver's watchdog stays fed.  Power-cycling TX while in
            // this state is the M4 exit-criterion scenario.
            Ok(None)
        }
    }

    async fn wait_ready(&mut self) {
        if (self.sent as usize) < CHORD_C_MAJOR.len() {
            return;
        }
        // No more events — block forever.  The TX loop's `select` pairs
        // this with the heartbeat timer, so heartbeats keep flowing.
        core::future::pending::<()>().await;
    }
}

// ── Synthetic source: scripted scenario walk ───────────────────────────────

/// A scripted source that cycles through a battery of realistic MIDI
/// scenarios for stress-testing the link layer:
///
/// 1. **C major scale** — eight notes up, recognisable melody.
/// 2. **Chord progression** (I-IV-V-I in C) — four held triads.
/// 3. **Glissando** — two octaves up then back down at ~30 ms/note.
/// 4. **Key smash** — eight-note cluster pressed within ~8 ms, held,
///    released en masse.
/// 5. **Quick stabs** — four staccato chords (50 ms hold each).
/// 6. **Pitch wheel** — full sweep up to max, down to min, back to centre.
/// 7. **Mod wheel** — full sweep on CC 1.
/// 8. **Mixer automation** — concurrent CC#7 / CC#10 / CC#64 tweens on
///    channels 0–2.  Exercises the "all CCs get delayed copies" path.
/// 9. **Patch walk** — sequence of `[Bank MSB][Bank LSB][PC]` triads
///    cycling through several patches on ch 0 and ch 9.  Exercises
///    latched-state reliability for the stuck-on-wrong-patch failure.
/// 10. **Aftertouch sweep** — held chord with Channel Pressure (0xDn)
///    sweeps and per-note Poly Aftertouch (0xAn) sweeps.  Exercises
///    both pressure paths and their dedup.
///
/// Between scenarios there's a 200 ms pause so the link runs essentially
/// continuously during walk-around tests.  After the last scenario, it
/// loops back to the first.
///
/// Each call to `next_message` waits until the next event's deadline
/// (using `embassy_time::Timer`) before returning bytes — so the
/// scheduling is realistic, not back-to-back.  This exercises
/// `MidiTxQueue` push timing, batch boundaries, real-time dedup, and
/// pitch-bend / CC override paths under load.
pub struct ScenarioSource {
    scenario: ScenarioId,
    step: usize,
    next_due: Option<Instant>,
    /// Track scenario starts so we only log once per transition.
    announced: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, defmt::Format)]
enum ScenarioId {
    Scale,
    ChordProgression,
    Glissando,
    KeySmash,
    QuickStabs,
    PitchWheel,
    ModWheel,
    MixerAutomation,
    PatchWalk,
    AftertouchSweep,
}

impl ScenarioId {
    const fn next(self) -> Self {
        match self {
            Self::Scale => Self::ChordProgression,
            Self::ChordProgression => Self::Glissando,
            Self::Glissando => Self::KeySmash,
            Self::KeySmash => Self::QuickStabs,
            Self::QuickStabs => Self::PitchWheel,
            Self::PitchWheel => Self::ModWheel,
            Self::ModWheel => Self::MixerAutomation,
            Self::MixerAutomation => Self::PatchWalk,
            Self::PatchWalk => Self::AftertouchSweep,
            Self::AftertouchSweep => Self::Scale,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum SynthEvent {
    NoteOn { ch: u8, note: u8, vel: u8 },
    NoteOff { ch: u8, note: u8 },
    PitchBend { ch: u8, value: u16 },
    ControlChange { ch: u8, ctrl: u8, value: u8 },
    ProgramChange { ch: u8, program: u8 },
    ChannelPressure { ch: u8, value: u8 },
    PolyAftertouch { ch: u8, note: u8, value: u8 },
}

impl SynthEvent {
    fn write(&self, buf: &mut [u8]) -> usize {
        match *self {
            Self::NoteOn { ch, note, vel } => {
                buf[0] = 0x90 | (ch & 0x0F);
                buf[1] = note & 0x7F;
                buf[2] = vel & 0x7F;
                3
            }
            Self::NoteOff { ch, note } => {
                buf[0] = 0x80 | (ch & 0x0F);
                buf[1] = note & 0x7F;
                buf[2] = 0;
                3
            }
            Self::PitchBend { ch, value } => {
                buf[0] = 0xE0 | (ch & 0x0F);
                buf[1] = (value & 0x7F) as u8;
                buf[2] = ((value >> 7) & 0x7F) as u8;
                3
            }
            Self::ControlChange { ch, ctrl, value } => {
                buf[0] = 0xB0 | (ch & 0x0F);
                buf[1] = ctrl & 0x7F;
                buf[2] = value & 0x7F;
                3
            }
            Self::ProgramChange { ch, program } => {
                buf[0] = 0xC0 | (ch & 0x0F);
                buf[1] = program & 0x7F;
                2
            }
            Self::ChannelPressure { ch, value } => {
                buf[0] = 0xD0 | (ch & 0x0F);
                buf[1] = value & 0x7F;
                2
            }
            Self::PolyAftertouch { ch, note, value } => {
                buf[0] = 0xA0 | (ch & 0x0F);
                buf[1] = note & 0x7F;
                buf[2] = value & 0x7F;
                3
            }
        }
    }
}

impl ScenarioSource {
    pub const fn new() -> Self {
        Self {
            scenario: ScenarioId::Scale,
            step: 0,
            next_due: None,
            announced: false,
        }
    }

    /// Compute the (event, delay-after-ms) for the current scenario step.
    /// Returns `None` if the scenario has finished.
    fn current(&self) -> Option<(SynthEvent, u32)> {
        match self.scenario {
            ScenarioId::Scale => scale_event(self.step),
            ScenarioId::ChordProgression => chord_progression_event(self.step),
            ScenarioId::Glissando => glissando_event(self.step),
            ScenarioId::KeySmash => key_smash_event(self.step),
            ScenarioId::QuickStabs => quick_stab_event(self.step),
            ScenarioId::PitchWheel => pitch_wheel_event(self.step),
            ScenarioId::ModWheel => mod_wheel_event(self.step),
            ScenarioId::MixerAutomation => mixer_automation_event(self.step),
            ScenarioId::PatchWalk => patch_walk_event(self.step),
            ScenarioId::AftertouchSweep => aftertouch_sweep_event(self.step),
        }
    }

    /// Advance to the next step, rolling over to the next scenario when
    /// the current one runs out.  Inserts a short 200 ms gap between
    /// scenarios so the link runs essentially continuously during walk
    /// tests but transitions remain visible in the log.
    fn advance(&mut self) {
        self.step += 1;
        if self.current().is_none() {
            self.scenario = self.scenario.next();
            self.step = 0;
            self.announced = false;
            self.next_due = Some(Instant::now() + Duration::from_millis(200));
        }
    }
}

impl Default for ScenarioSource {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, defmt::Format)]
pub enum ScenarioSourceError {}

impl MidiSource for ScenarioSource {
    type Error = ScenarioSourceError;

    fn try_next(&mut self, buf: &mut [u8]) -> Result<Option<usize>, Self::Error> {
        // If we're waiting for a deadline, check it.  No `Timer::at`
        // here — this is called from sync drain context.
        if let Some(due) = self.next_due {
            if Instant::now() < due {
                return Ok(None);
            }
            self.next_due = None;
        }
        if !self.announced {
            defmt::info!("scenario source: starting {}", self.scenario);
            self.announced = true;
        }
        if let Some((event, delay_ms)) = self.current() {
            let n = event.write(buf);
            self.next_due = Some(Instant::now() + Duration::from_millis(delay_ms as u64));
            self.advance();
            return Ok(Some(n));
        }
        // Defensive: if a scenario unexpectedly produces no events,
        // skip forward rather than spin.
        self.scenario = self.scenario.next();
        self.step = 0;
        self.announced = false;
        Ok(None)
    }

    async fn wait_ready(&mut self) {
        if let Some(due) = self.next_due {
            if Instant::now() >= due {
                return;
            }
            // Timer::at is safe here — the TX loop only awaits this
            // inside `select`, where the executor's waker is real.
            Timer::at(due).await;
        }
        // No deadline = always ready (e.g., between scenarios where the
        // 1.5 s pause has already passed, or first event of a session).
    }
}

// ── Per-scenario step generators ───────────────────────────────────────────

/// C major scale up: C D E F G A B C, then descending.
fn scale_event(step: usize) -> Option<(SynthEvent, u32)> {
    const NOTES_UP: &[u8] = &[60, 62, 64, 65, 67, 69, 71, 72];
    // step layout: NoteOn, NoteOff per note, then descending NoteOn, NoteOff per note.
    let total_notes = NOTES_UP.len() * 2; // up + down
    let pair = step / 2;
    if pair >= total_notes {
        return None;
    }
    let note = if pair < NOTES_UP.len() {
        NOTES_UP[pair]
    } else {
        NOTES_UP[NOTES_UP.len() - 1 - (pair - NOTES_UP.len())]
    };
    if step % 2 == 0 {
        Some((
            SynthEvent::NoteOn {
                ch: 0,
                note,
                vel: 96,
            },
            180,
        ))
    } else {
        Some((SynthEvent::NoteOff { ch: 0, note }, 30))
    }
}

/// I-IV-V-I in C major.  Each chord: three NoteOns within ~6 ms, hold
/// 700 ms, three NoteOffs within ~6 ms, then a 200 ms gap before the
/// next chord.
fn chord_progression_event(step: usize) -> Option<(SynthEvent, u32)> {
    const CHORDS: &[[u8; 3]] = &[
        [60, 64, 67], // I:  C major
        [65, 69, 72], // IV: F major
        [67, 71, 74], // V:  G major
        [60, 64, 67], // I:  C major
    ];
    const STEPS_PER_CHORD: usize = 6; // 3 ons + 3 offs
    if step >= CHORDS.len() * STEPS_PER_CHORD {
        return None;
    }
    let chord = &CHORDS[step / STEPS_PER_CHORD];
    let local = step % STEPS_PER_CHORD;
    let (note, is_on, last_in_phase) = match local {
        0 => (chord[0], true, false),
        1 => (chord[1], true, false),
        2 => (chord[2], true, true), // last NoteOn — long hold after
        3 => (chord[0], false, false),
        4 => (chord[1], false, false),
        5 => (chord[2], false, true), // last NoteOff — gap before next chord
        _ => unreachable!(),
    };
    let delay_ms = if is_on && last_in_phase {
        700 // hold the chord
    } else if !is_on && last_in_phase {
        200 // gap between chords
    } else {
        3 // tight inter-note (within phase)
    };
    if is_on {
        Some((
            SynthEvent::NoteOn {
                ch: 0,
                note,
                vel: 100,
            },
            delay_ms,
        ))
    } else {
        Some((SynthEvent::NoteOff { ch: 0, note }, delay_ms))
    }
}

/// 24-semitone glissando up from C4 (60) to C6 (84), then back down.
/// Each note: NoteOn → 25 ms → NoteOff → 5 ms → next note.
fn glissando_event(step: usize) -> Option<(SynthEvent, u32)> {
    const SPAN: usize = 24; // semitones, C4..C6
    let direction_steps = SPAN * 2; // NoteOn + NoteOff per note
    let total = direction_steps * 2; // ascending + descending
    if step >= total {
        return None;
    }
    let direction_up = step < direction_steps;
    let local = if direction_up {
        step
    } else {
        step - direction_steps
    };
    let pair = local / 2;
    let is_on = local % 2 == 0;
    let note: u8 = if direction_up {
        60 + pair as u8
    } else {
        84 - pair as u8
    };
    if is_on {
        Some((
            SynthEvent::NoteOn {
                ch: 0,
                note,
                vel: 80,
            },
            25,
        ))
    } else {
        Some((SynthEvent::NoteOff { ch: 0, note }, 5))
    }
}

/// Eight-note cluster smashed within ~8 ms.  Hold 500 ms.  Release all
/// within ~8 ms.  Stresses chord batching, large-burst event_seq
/// allocation, and end-to-end NoteOff cancellation.
fn key_smash_event(step: usize) -> Option<(SynthEvent, u32)> {
    const NOTES: &[u8] = &[60, 62, 64, 65, 67, 69, 71, 72];
    let n = NOTES.len();
    if step < n {
        // Press: each NoteOn is 1 ms apart, last one holds 500 ms.
        let delay = if step == n - 1 { 500 } else { 1 };
        Some((
            SynthEvent::NoteOn {
                ch: 0,
                note: NOTES[step],
                vel: 110,
            },
            delay,
        ))
    } else if step < 2 * n {
        // Release: each NoteOff is 1 ms apart, last one is followed by
        // a 200 ms gap.
        let idx = step - n;
        let delay = if idx == n - 1 { 200 } else { 1 };
        Some((
            SynthEvent::NoteOff {
                ch: 0,
                note: NOTES[idx],
            },
            delay,
        ))
    } else {
        None
    }
}

/// Four staccato chord stabs at 200 ms intervals.  Each stab is a triad
/// pressed within ~3 ms, held 50 ms, released within ~3 ms.  Tests the
/// "NoteOff cancels partially-transmitted NoteOn" path on every stab.
fn quick_stab_event(step: usize) -> Option<(SynthEvent, u32)> {
    const STABS: usize = 4;
    const PER_STAB: usize = 6;
    if step >= STABS * PER_STAB {
        return None;
    }
    let stab_idx = step / PER_STAB;
    let local = step % PER_STAB;
    let root: u8 = match stab_idx {
        0 => 60, // C major triad
        1 => 65, // F major triad
        2 => 67, // G major triad
        _ => 60, // C major again
    };
    let third: u8 = root + 4;
    let fifth: u8 = root + 7;
    match local {
        0 => Some((
            SynthEvent::NoteOn {
                ch: 0,
                note: root,
                vel: 100,
            },
            1,
        )),
        1 => Some((
            SynthEvent::NoteOn {
                ch: 0,
                note: third,
                vel: 100,
            },
            1,
        )),
        2 => Some((
            SynthEvent::NoteOn {
                ch: 0,
                note: fifth,
                vel: 100,
            },
            50,
        )),
        3 => Some((SynthEvent::NoteOff { ch: 0, note: root }, 1)),
        4 => Some((SynthEvent::NoteOff { ch: 0, note: third }, 1)),
        5 => Some((SynthEvent::NoteOff { ch: 0, note: fifth }, 200)),
        _ => unreachable!(),
    }
}

/// Pitch-bend sweep: centre → max → min → centre.  ~5 ms between events
/// = ~200 events/sec.  Tests the "PB on same channel cancels prior PB"
/// dedup path under high event rate (queue depth stays ≈ 1).
fn pitch_wheel_event(step: usize) -> Option<(SynthEvent, u32)> {
    const STEPS_UP: usize = 80; // centre (8192) → max (16383)
    const STEPS_DOWN: usize = 160; // max → 0
    const STEPS_RECOVER: usize = 80; // 0 → centre
    const TOTAL: usize = STEPS_UP + STEPS_DOWN + STEPS_RECOVER;
    if step >= TOTAL {
        return None;
    }
    let value: u16 = if step < STEPS_UP {
        // Linear ramp 8192 → 16383
        let ratio = (step + 1) as u32 * 8191 / STEPS_UP as u32;
        8192 + ratio as u16
    } else if step < STEPS_UP + STEPS_DOWN {
        let s = step - STEPS_UP;
        let ratio = s as u32 * 16383 / STEPS_DOWN as u32;
        16383u16.saturating_sub(ratio as u16)
    } else {
        let s = step - STEPS_UP - STEPS_DOWN;
        let ratio = (s + 1) as u32 * 8192 / STEPS_RECOVER as u32;
        ratio as u16
    };
    Some((SynthEvent::PitchBend { ch: 0, value }, 5))
}

/// Mod-wheel sweep on CC 1: 0 → 127 → 0.  ~5 ms between events.  Tests
/// the "CC on same channel + same controller cancels prior" dedup path.
fn mod_wheel_event(step: usize) -> Option<(SynthEvent, u32)> {
    const HALF: usize = 64;
    const TOTAL: usize = HALF * 2;
    if step >= TOTAL {
        return None;
    }
    let value: u8 = if step < HALF {
        ((step + 1) as u32 * 127 / HALF as u32) as u8
    } else {
        let s = step - HALF;
        127u8.saturating_sub((s as u32 * 127 / HALF as u32) as u8)
    };
    Some((
        SynthEvent::ControlChange {
            ch: 0,
            ctrl: 1,
            value,
        },
        5,
    ))
}

/// Concurrent mixer automation: tweens CC#7 (volume) and CC#10 (pan) on
/// channels 0–2, with CC#64 (sustain) toggling on ch 0.  Each "frame"
/// emits 7 events (3 vol + 3 pan + occasional sustain) at ~30 ms cadence.
/// Tests the new "all CCs get delayed copies" path with concurrent sweeps
/// across channels and controllers — the steady-state wire bandwidth
/// should stay close to K=3 because dedup_for_incoming cancels stale
/// pending values per (ch, cc#).
fn mixer_automation_event(step: usize) -> Option<(SynthEvent, u32)> {
    const FRAMES: usize = 32; // ~32 frames × ~210 ms ≈ 6.7 s
    const EVENTS_PER_FRAME: usize = 7;
    if step >= FRAMES * EVENTS_PER_FRAME {
        return None;
    }
    let frame = step / EVENTS_PER_FRAME;
    let local = step % EVENTS_PER_FRAME;
    // Phase the three channel tweens so they don't all peak together.
    // Volume = triangle wave, Pan = sine-ish via simple folding.
    let triangle = |t: usize, period: usize| -> u8 {
        let p = (t * 2) % period;
        let half = period / 2;
        if p < half {
            (p as u32 * 127 / half as u32) as u8
        } else {
            let q = p - half;
            127u8.saturating_sub((q as u32 * 127 / half as u32) as u8)
        }
    };
    let vol = |ch: u8| triangle(frame + ch as usize * 4, FRAMES);
    let pan = |ch: u8| triangle(frame + ch as usize * 4 + 8, FRAMES);

    // Inter-event delay within a frame is short (5 ms); the last event
    // of the frame holds for ~180 ms before the next frame begins.
    let last_in_frame = local == EVENTS_PER_FRAME - 1;
    let delay = if last_in_frame { 180 } else { 5 };
    let event = match local {
        0 => SynthEvent::ControlChange {
            ch: 0,
            ctrl: 7,
            value: vol(0),
        },
        1 => SynthEvent::ControlChange {
            ch: 1,
            ctrl: 7,
            value: vol(1),
        },
        2 => SynthEvent::ControlChange {
            ch: 2,
            ctrl: 7,
            value: vol(2),
        },
        3 => SynthEvent::ControlChange {
            ch: 0,
            ctrl: 10,
            value: pan(0),
        },
        4 => SynthEvent::ControlChange {
            ch: 1,
            ctrl: 10,
            value: pan(1),
        },
        5 => SynthEvent::ControlChange {
            ch: 2,
            ctrl: 10,
            value: pan(2),
        },
        // Sustain pedal on ch 0: press for the first half, release the
        // second.  Tests that CC#64 latched-state survives the link.
        6 => {
            let value: u8 = if frame < FRAMES / 2 { 127 } else { 0 };
            SynthEvent::ControlChange {
                ch: 0,
                ctrl: 64,
                value,
            }
        }
        _ => unreachable!(),
    };
    Some((event, delay))
}

/// Patch walk: cycle through several patches on ch 0 and ch 9, sending
/// `[Bank MSB][Bank LSB][PC][NoteOn..NoteOff]` for each.  The note plays
/// briefly so the patch change is audible end-to-end if any synth is
/// hooked up.  This is the "stuck on wrong patch" failure mode the
/// delayed-copy retransmits are meant to defend against — every PC and
/// CC#0/CC#32 here gets the full main-K=3 + 2× delayed-copy reliability.
fn patch_walk_event(step: usize) -> Option<(SynthEvent, u32)> {
    // (channel, bank_msb, bank_lsb, program, note)
    const PATCHES: &[(u8, u8, u8, u8, u8)] = &[
        (0, 0, 0, 0, 60),    // ch 0, GM Acoustic Grand
        (0, 0, 0, 32, 60),   // ch 0, GM Acoustic Bass
        (0, 0, 0, 56, 67),   // ch 0, GM Trumpet
        (0, 121, 0, 73, 72), // ch 0, MSB-only bank (GM2 Pan Flute, fictional)
        (9, 0, 0, 0, 36),    // ch 9 (drums), kit 0, kick
        (9, 0, 0, 16, 38),   // ch 9, power kit, snare
    ];
    const STEPS_PER_PATCH: usize = 5; // MSB, LSB, PC, NoteOn, NoteOff
    if step >= PATCHES.len() * STEPS_PER_PATCH {
        return None;
    }
    let (ch, msb, lsb, program, note) = PATCHES[step / STEPS_PER_PATCH];
    let local = step % STEPS_PER_PATCH;
    match local {
        0 => Some((
            SynthEvent::ControlChange {
                ch,
                ctrl: 0,
                value: msb,
            },
            3,
        )),
        1 => Some((
            SynthEvent::ControlChange {
                ch,
                ctrl: 32,
                value: lsb,
            },
            3,
        )),
        2 => Some((SynthEvent::ProgramChange { ch, program }, 50)),
        3 => Some((SynthEvent::NoteOn { ch, note, vel: 100 }, 250)),
        // Hold each patch's test note for 250 ms; gap 250 ms before
        // moving to the next patch.
        4 => Some((SynthEvent::NoteOff { ch, note }, 250)),
        _ => unreachable!(),
    }
}

/// Aftertouch sweep: press a 3-note chord on ch 0, then sweep Channel
/// Pressure (0xDn) up and down across the held chord, then sweep Poly
/// Aftertouch (0xAn) on each note in turn, then release the chord.
/// Tests both pressure paths and their dedup:
/// * CP cancels by channel only — repeated CP values should dedup
///   pending stale copies the same way pitch bend does.
/// * PolyAT cancels by (ch, note) — per-note sweeps on different notes
///   should not interfere.
fn aftertouch_sweep_event(step: usize) -> Option<(SynthEvent, u32)> {
    const CHORD: &[u8] = &[60, 64, 67]; // C major triad on ch 0
    const CP_STEPS: usize = 32; // 16 up + 16 down
    const PAT_STEPS_PER_NOTE: usize = 16; // 8 up + 8 down per note
    const PAT_TOTAL: usize = PAT_STEPS_PER_NOTE * 3;
    const PRESS: usize = 3;
    const RELEASE: usize = 3;
    const TOTAL: usize = PRESS + CP_STEPS + PAT_TOTAL + RELEASE;
    if step >= TOTAL {
        return None;
    }

    // Phase 1: NoteOn the chord, last NoteOn holds 50 ms before CP starts.
    if step < PRESS {
        let last = step == PRESS - 1;
        let delay = if last { 50 } else { 3 };
        return Some((
            SynthEvent::NoteOn {
                ch: 0,
                note: CHORD[step],
                vel: 100,
            },
            delay,
        ));
    }

    // Phase 2: Channel Pressure sweep 0 → 127 → 0 across ch 0.
    let after_press = step - PRESS;
    if after_press < CP_STEPS {
        let half = CP_STEPS / 2;
        let value: u8 = if after_press < half {
            ((after_press + 1) as u32 * 127 / half as u32) as u8
        } else {
            let s = after_press - half;
            127u8.saturating_sub((s as u32 * 127 / half as u32) as u8)
        };
        let last = after_press == CP_STEPS - 1;
        let delay = if last { 30 } else { 8 };
        return Some((SynthEvent::ChannelPressure { ch: 0, value }, delay));
    }

    // Phase 3: PolyAftertouch — sweep 0 → 127 → 0 on each chord note in turn.
    let after_cp = after_press - CP_STEPS;
    if after_cp < PAT_TOTAL {
        let note_idx = after_cp / PAT_STEPS_PER_NOTE;
        let local = after_cp % PAT_STEPS_PER_NOTE;
        let half = PAT_STEPS_PER_NOTE / 2;
        let value: u8 = if local < half {
            ((local + 1) as u32 * 127 / half as u32) as u8
        } else {
            let s = local - half;
            127u8.saturating_sub((s as u32 * 127 / half as u32) as u8)
        };
        let last_in_note = local == PAT_STEPS_PER_NOTE - 1;
        let delay = if last_in_note { 30 } else { 8 };
        return Some((
            SynthEvent::PolyAftertouch {
                ch: 0,
                note: CHORD[note_idx],
                value,
            },
            delay,
        ));
    }

    // Phase 4: NoteOff the chord, last NoteOff holds 200 ms before scenario ends.
    let after_pat = after_cp - PAT_TOTAL;
    let last = after_pat == RELEASE - 1;
    let delay = if last { 200 } else { 3 };
    Some((
        SynthEvent::NoteOff {
            ch: 0,
            note: CHORD[after_pat],
        },
        delay,
    ))
}

// ── Synthetic sink: defmt logger ────────────────────────────────────────────

/// Sink that logs incoming MIDI + all-notes-off events via defmt.  Swap for
/// a UART-backed sink when the MIDI FeatherWing arrives.
pub struct DefmtLogSink;

#[derive(Debug, Clone, Copy, defmt::Format)]
pub enum DefmtSinkError {}

impl MidiSink for DefmtLogSink {
    type Error = DefmtSinkError;

    async fn write_message(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        defmt::info!("MIDI OUT: {=[u8]:#x}", bytes);
        Ok(())
    }

    async fn all_notes_off(&mut self) -> Result<(), Self::Error> {
        defmt::warn!("MIDI OUT: ALL_NOTES_OFF (16 channels × CC 123 = 0)");
        // A real UART sink would write the 48 bytes to the MIDI port:
        //   for ch in 0..16 { uart.write([0xB0 | ch, 0x7B, 0x00]); }
        // Logging here is enough to confirm watchdog → all-notes-off
        // pipeline at the link layer.
        Ok(())
    }
}
