use crate::config::Config;
use cairo::{Context, Surface};
use drm::control::ClipRect;
use std::time::{Duration, Instant};

/// Backend a slider drives. `Brightness` writes the display backlight directly
/// via sysfs; a future `Volume` will go out over the control socket.
pub enum SliderBackend {
    Brightness,
}

/// A full-bar slider widget: tap-and-hold a trigger button to expand into it,
/// drag to set, release to collapse. Holds display state only — the touch
/// dispatch anchors the grab, feeds drag deltas in, and pushes the level to the
/// backend.
pub struct Slider {
    pub backend: SliderBackend,
    /// Current level, 0.0..=1.0.
    pub level: f64,
    /// (grab_x in device coords, grab_level) recorded on entry; None when idle.
    grab: Option<(f64, f64)>,
    pub changed: bool,
    /// Level at the last draw, so a partial redraw only damages the strip the
    /// fill edge moved across (full-panel damage is ~1s on appletbdrm; small ~1ms).
    drawn_level: f64,
    /// Time of the last repaint, to throttle the fill to ~30fps — the device can't
    /// take a push per drag sample.
    last_draw: Instant,
}

impl Slider {
    pub fn new(backend: SliderBackend) -> Slider {
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
    pub fn grab(&mut self, x: f64, level: f64) {
        self.grab = Some((x, level));
        self.level = level;
        self.changed = true;
    }
    /// Update from the current touch x; returns the new level so the caller can
    /// push it to the backend. `width` is the bar's long-axis length.
    pub fn drag(&mut self, x: f64, width: f64) -> f64 {
        if let Some((gx, gl)) = self.grab {
            let new = (gl + (x - gx) / width).clamp(0.0, 1.0);
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
    pub fn release(&mut self) {
        self.grab = None;
    }
    pub fn draw(
        &mut self,
        _config: &Config,
        width: i32,
        height: i32,
        surface: &Surface,
        _pixel_shift: (f64, f64),
        complete_redraw: bool,
    ) -> Vec<ClipRect> {
        let c = Context::new(surface).unwrap();
        c.translate(height as f64, 0.0);
        c.rotate((90.0f64).to_radians());
        let margin = 24.0;
        let track_w = width as f64 - 2.0 * margin;
        let track_h = height as f64 * 0.35;
        let tx = margin;
        let ty = (height as f64 - track_h) / 2.0;
        // Only clear the whole surface on a full redraw (modal entry); on drag
        // frames we just repaint the track in place.
        if complete_redraw {
            c.set_source_rgb(0.0, 0.0, 0.0);
            c.paint().unwrap();
        }
        // Dim background track.
        c.set_source_rgb(0.22, 0.22, 0.22);
        c.rectangle(tx, ty, track_w, track_h);
        c.fill().unwrap();
        // Bright filled portion proportional to the level.
        let fill_w = (track_w * self.level).clamp(0.0, track_w);
        c.set_source_rgb(1.0, 1.0, 1.0);
        c.rectangle(tx, ty, fill_w, track_h);
        c.fill().unwrap();

        // Damage the whole bar on entry; otherwise only the long-axis strip the
        // fill edge swept across this frame — full-panel pushes take ~1s here.
        let clips = if complete_redraw {
            vec![ClipRect::new(0, 0, height as u16, width as u16)]
        } else {
            let lo =
                (tx + track_w * self.drawn_level.min(self.level) - 4.0).clamp(0.0, width as f64);
            let hi =
                (tx + track_w * self.drawn_level.max(self.level) + 4.0).clamp(0.0, width as f64);
            vec![ClipRect::new(
                (height as f64 - (ty + track_h)).max(0.0) as u16,
                lo as u16,
                (height as f64 - ty) as u16,
                hi as u16,
            )]
        };
        self.drawn_level = self.level;
        self.changed = false;
        self.last_draw = Instant::now();
        clips
    }
}
