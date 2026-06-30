use crate::action::Action;
use crate::state::State;
use crate::widgets::SliderBackend;
use cairo::Context;
use drm::control::ClipRect;
use std::time::{Duration, Instant};

/// The slider track occupies this band of the bar's long axis (fractions). A
/// short, right-of-centre strip rather than the whole bar, so a full traverse of
/// the visible track covers the full 0.0..=1.0 range — drastic changes need only
/// a short swipe.
const SLIDER_TRACK_START: f64 = 0.60;
const SLIDER_TRACK_END: f64 = 0.90;

/// Track thickness and the white thumb's size, as fractions of the bar's short
/// axis. The track is thin; the thumb is a taller rounded handle at the value.
const SLIDER_TRACK_HEIGHT_FRAC: f64 = 0.25;
const SLIDER_THUMB_HEIGHT_FRAC: f64 = 0.55;
const SLIDER_THUMB_WIDTH_FRAC: f64 = 0.20;

/// The filled portion of the track.
const SLIDER_FILL_RGB: (f64, f64, f64) = (0.20, 0.55, 1.0);
/// The unfilled track behind it.
const SLIDER_TRACK_RGB: (f64, f64, f64) = (0.20, 0.20, 0.22);

/// Trace a rounded-rectangle path; the caller sets the source and fills. `r` is
/// clamped so it never exceeds half the shorter side.
fn rounded_rect(c: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    c.new_sub_path();
    c.arc(x + w - r, y + r, r, (-90f64).to_radians(), 0f64.to_radians());
    c.arc(x + w - r, y + h - r, r, 0f64.to_radians(), 90f64.to_radians());
    c.arc(x + r, y + h - r, r, 90f64.to_radians(), 180f64.to_radians());
    c.arc(x + r, y + r, r, 180f64.to_radians(), 270f64.to_radians());
    c.close_path();
}

/// Brightness slider effect: pushes the level to the display backlight and reads
/// its initial value back from the App-owned `State` on grab.
pub(crate) struct BrightnessSlider;

impl SliderBackend for BrightnessSlider {
    fn apply(&self, level: f64) -> Action {
        Action::SetBrightness(level)
    }
    fn initial_level(&self, s: &State) -> f64 {
        s.brightness
    }
}

/// Keyboard-illumination slider effect: pushes the level to the keyboard
/// backlight LED and reads its current value back from `State` on grab.
pub(crate) struct KbdIllumSlider;

impl SliderBackend for KbdIllumSlider {
    fn apply(&self, level: f64) -> Action {
        Action::SetKbdIllum(level)
    }
    fn initial_level(&self, s: &State) -> f64 {
        s.kbd_illum
    }
}

/// Volume slider effect: pushes the level to the ALSA Master mixer and reads its
/// current value back from `State` on grab.
pub(crate) struct VolumeSlider;

impl SliderBackend for VolumeSlider {
    fn apply(&self, level: f64) -> Action {
        Action::SetVolume(level)
    }
    fn initial_level(&self, s: &State) -> f64 {
        s.volume
    }
}

/// A full-bar slider widget: tap-and-hold a trigger button to expand into it,
/// drag to set, release to collapse. Holds the shared drag UI only — the touch
/// dispatch anchors the grab, feeds drag deltas in, and pushes the level to the
/// backend (which decides the effect).
pub(crate) struct Slider {
    pub(crate) backend: Box<dyn SliderBackend>,
    /// Current level, 0.0..=1.0.
    level: f64,
    /// (grab_x in device coords, grab_level) recorded on entry; None when idle.
    grab: Option<(f64, f64)>,
    pub(crate) changed: bool,
    /// Level at the last draw, so a partial redraw only damages the strip the
    /// fill edge moved across (full-panel damage is ~1s on appletbdrm; small ~1ms).
    drawn_level: f64,
    /// Time of the last repaint, to throttle the fill to ~30fps — the device can't
    /// take a push per drag sample.
    last_draw: Instant,
}

impl Slider {
    pub(crate) fn new(backend: Box<dyn SliderBackend>) -> Slider {
        Slider {
            backend,
            level: 0.0,
            grab: None,
            changed: true,
            drawn_level: 0.0,
            last_draw: Instant::now(),
        }
    }
    /// Begin a drag: anchor at the current touch x and the current level so the
    /// fill tracks relative to where you grabbed (no jump to the finger).
    pub(crate) fn grab(&mut self, x: f64, level: f64) {
        self.grab = Some((x, level));
        self.level = level;
        self.changed = true;
    }
    /// Update from the current touch x; returns the new level so the caller can
    /// push it to the backend. `width` is the bar's long-axis length.
    pub(crate) fn drag(&mut self, x: f64, width: f64) -> f64 {
        if let Some((gx, gl)) = self.grab {
            // Divide the finger offset by the band width (not the full bar) so a
            // full-band swipe spans the whole range — the sensitivity win.
            let band_w = width * (SLIDER_TRACK_END - SLIDER_TRACK_START);
            let new = (gl + (x - gx) / band_w).clamp(0.0, 1.0);
            if (new - self.level).abs() > f64::EPSILON {
                self.level = new;
                // Throttle repaints to ~30fps; brightness still updates every frame
                // (the caller applies `level` directly), only the fill is limited.
                if self.last_draw.elapsed() >= Duration::from_millis(33) {
                    self.changed = true;
                }
            }
        }
        self.level
    }
    pub(crate) fn release(&mut self) {
        self.grab = None;
    }
    /// Draw into the already-rotated layer context. `width`/`height` are the
    /// bar's dimensions; the track is drawn in the `[SLIDER_TRACK_START,
    /// SLIDER_TRACK_END]` band of the long axis (the rest of the bar stays black),
    /// ignoring per-cell geometry and pixel shift.
    pub(crate) fn draw(
        &mut self,
        c: &Context,
        width: i32,
        height: i32,
        complete_redraw: bool,
    ) -> Vec<ClipRect> {
        let tx = width as f64 * SLIDER_TRACK_START;
        let track_w = width as f64 * (SLIDER_TRACK_END - SLIDER_TRACK_START);
        let track_h = height as f64 * SLIDER_TRACK_HEIGHT_FRAC;
        let ty = (height as f64 - track_h) / 2.0;
        let track_r = track_h / 2.0;
        let thumb_h = height as f64 * SLIDER_THUMB_HEIGHT_FRAC;
        let thumb_w = height as f64 * SLIDER_THUMB_WIDTH_FRAC;
        let thumb_ty = (height as f64 - thumb_h) / 2.0;
        let fill_w = (track_w * self.level).clamp(0.0, track_w);

        // Long-axis strip the thumb swept this frame — used both to clear the old
        // thumb and to bound the damage push.
        let lo = (tx + track_w * self.drawn_level.min(self.level) - thumb_w / 2.0 - 2.0)
            .clamp(0.0, width as f64);
        let hi = (tx + track_w * self.drawn_level.max(self.level) + thumb_w / 2.0 + 2.0)
            .clamp(0.0, width as f64);

        if complete_redraw {
            // Modal entry: clear the whole surface.
            c.set_source_rgb(0.0, 0.0, 0.0);
            c.paint().unwrap();
        } else {
            // Drag frame: clear the swept strip at the thumb's FULL height. The
            // thumb is taller than the track, so repainting the thin track alone
            // leaves the old thumb's protruding ends behind (the smear).
            c.set_source_rgb(0.0, 0.0, 0.0);
            c.rectangle(lo, thumb_ty, hi - lo, thumb_h);
            c.fill().unwrap();
        }
        // Dim rounded track.
        c.set_source_rgb(SLIDER_TRACK_RGB.0, SLIDER_TRACK_RGB.1, SLIDER_TRACK_RGB.2);
        rounded_rect(c, tx, ty, track_w, track_h, track_r);
        c.fill().unwrap();
        // Blue fill up to the level: a square-right rectangle clipped to the
        // track's rounded shape, so its left end is rounded and its right end sits
        // flush under the thumb.
        c.save().unwrap();
        rounded_rect(c, tx, ty, track_w, track_h, track_r);
        c.clip();
        c.set_source_rgb(SLIDER_FILL_RGB.0, SLIDER_FILL_RGB.1, SLIDER_FILL_RGB.2);
        c.rectangle(tx, ty, fill_w, track_h);
        c.fill().unwrap();
        c.restore().unwrap();
        // White rounded thumb at the current value.
        c.set_source_rgb(1.0, 1.0, 1.0);
        rounded_rect(
            c,
            tx + fill_w - thumb_w / 2.0,
            thumb_ty,
            thumb_w,
            thumb_h,
            thumb_w / 2.0,
        );
        c.fill().unwrap();

        // Damage the whole bar on entry; otherwise only the strip the thumb swept
        // (plus its half-width), over the thumb's full height (the tallest element).
        let clips = if complete_redraw {
            vec![ClipRect::new(0, 0, height as u16, width as u16)]
        } else {
            vec![ClipRect::new(
                (height as f64 - (thumb_ty + thumb_h)).max(0.0) as u16,
                lo as u16,
                (height as f64 - thumb_ty) as u16,
                hi as u16,
            )]
        };
        self.drawn_level = self.level;
        self.changed = false;
        self.last_draw = Instant::now();
        clips
    }
}
