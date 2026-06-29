use cairo::{Context, Surface};
use drm::control::ClipRect;

use crate::config::{ButtonConfig, Config};
use crate::pixel_shift::PIXEL_SHIFT_WIDTH_PX;
use crate::widgets::{Button, Cell, Region, Widget};
use crate::BUTTON_SPACING_PX;

#[derive(Default)]
pub struct FunctionLayer {
    displays_time: bool,
    displays_battery: bool,
    pub(crate) cells: Vec<Cell>,
    virtual_button_count: usize,
    faster_refresh: bool,
}

impl FunctionLayer {
    pub(crate) fn with_config(cfg: Vec<ButtonConfig>) -> FunctionLayer {
        if cfg.is_empty() {
            panic!("Invalid configuration, layer has 0 buttons");
        }

        // Keep the battery flag config-based so the "no battery device" fallback
        // (a "Battery N/A" button) still reports as a battery layer, as before.
        let displays_battery = cfg.iter().any(|cfg| cfg.battery.is_some());
        let mut virtual_button_count = 0;
        let cells = cfg
            .into_iter()
            .map(|cfg| {
                let mut stretch = cfg.stretch.unwrap_or(1);
                if stretch < 1 {
                    println!("Stretch value must be at least 1, setting to 1.");
                    stretch = 1;
                }
                virtual_button_count += stretch;
                Cell {
                    stretch,
                    widget: Widget::from_config(cfg),
                }
            })
            .collect::<Vec<_>>();
        let displays_time = cells.iter().any(|c| c.widget.is_clock());
        let faster_refresh = cells.iter().any(|c| c.widget.needs_faster_refresh());
        FunctionLayer {
            displays_time,
            displays_battery,
            cells,
            virtual_button_count,
            faster_refresh,
        }
    }
    pub(crate) fn faster_refresh(&self) -> bool {
        self.faster_refresh
    }
    pub(crate) fn displays_time(&self) -> bool {
        self.displays_time
    }
    pub(crate) fn displays_battery(&self) -> bool {
        self.displays_battery
    }
    pub(crate) fn any_changed(&self) -> bool {
        self.cells.iter().any(|c| c.widget.changed())
    }
    pub(crate) fn mark_batteries_changed(&mut self) {
        for cell in &mut self.cells {
            if cell.widget.is_battery() {
                cell.widget.mark_changed();
            }
        }
    }
    /// Mutable access to the button in cell `i`, if that cell holds a real
    /// button (used by the touch loop to toggle its active state).
    pub(crate) fn button_mut(&mut self, i: usize) -> Option<&mut Button> {
        match self.cells.get_mut(i).map(|c| &mut c.widget) {
            Some(Widget::Button(b)) => Some(b),
            _ => None,
        }
    }
    /// The modal layer cell `i` opens on touch, if it holds a button with an
    /// `open_layer` set.
    pub(crate) fn open_layer(&self, i: usize) -> Option<String> {
        match self.cells.get(i).map(|c| &c.widget) {
            Some(Widget::Button(b)) => b.open_layer.clone(),
            _ => None,
        }
    }
    pub(crate) fn draw(
        &mut self,
        config: &Config,
        width: i32,
        height: i32,
        surface: &Surface,
        pixel_shift: (f64, f64),
        complete_redraw: bool,
    ) -> Vec<ClipRect> {
        let c = Context::new(surface).unwrap();
        let mut modified_regions = if complete_redraw {
            vec![ClipRect::new(0, 0, height as u16, width as u16)]
        } else {
            Vec::new()
        };
        c.translate(height as f64, 0.0);
        c.rotate((90.0f64).to_radians());
        let pixel_shift_width = if config.enable_pixel_shift {
            PIXEL_SHIFT_WIDTH_PX
        } else {
            0
        };
        let virtual_button_width = ((width - pixel_shift_width as i32)
            - (BUTTON_SPACING_PX * (self.virtual_button_count - 1) as i32))
            as f64
            / self.virtual_button_count as f64;
        let (pixel_shift_x, pixel_shift_y) = pixel_shift;

        if complete_redraw {
            c.set_source_rgb(0.0, 0.0, 0.0);
            c.paint().unwrap();
        }
        c.set_font_face(&config.font_face);
        c.set_font_size(32.0);

        let mut start = 0;
        for cell in &mut self.cells {
            let end = start + cell.stretch;

            if !cell.widget.changed() && !complete_redraw {
                start = end;
                continue;
            };

            let left_edge = (start as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64))
                .floor()
                + pixel_shift_x
                + (pixel_shift_width / 2) as f64;

            let button_width = virtual_button_width
                + ((end - start - 1) as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64))
                    .floor();

            let region = Region {
                left: left_edge,
                width: button_width,
                height,
                y_shift: pixel_shift_y,
            };
            modified_regions.extend(cell.widget.draw(&c, config, &region, complete_redraw));
            start = end;
        }

        modified_regions
    }

    pub(crate) fn hit(&self, width: u16, height: u16, x: f64, y: f64, i: Option<usize>) -> Option<usize> {
        let virtual_button_width =
            (width as i32 - (BUTTON_SPACING_PX * (self.virtual_button_count - 1) as i32)) as f64
                / self.virtual_button_count as f64;

        let i = i.unwrap_or_else(|| {
            let virtual_i = (x / (width as f64 / self.virtual_button_count as f64)) as usize;
            // Find the cell whose virtual span contains `virtual_i`.
            let mut acc = 0;
            self.cells
                .iter()
                .position(|cell| {
                    acc += cell.stretch;
                    acc > virtual_i
                })
                .unwrap_or(self.cells.len() - 1)
        });
        if i >= self.cells.len() {
            return None;
        }

        // Cumulative virtual start of cell `i`.
        let start: usize = self.cells[..i].iter().map(|c| c.stretch).sum();
        let end = start + self.cells[i].stretch;

        let left_edge = (start as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64)).floor();

        let button_width = virtual_button_width
            + ((end - start - 1) as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64))
                .floor();

        if x < left_edge
            || x > (left_edge + button_width)
            || y < 0.1 * height as f64
            || y > 0.9 * height as f64
        {
            return None;
        }

        // Indicators and spacers are passive: a touch on them is a miss.
        if !self.cells[i].widget.interactive() {
            return None;
        }

        Some(i)
    }
}
