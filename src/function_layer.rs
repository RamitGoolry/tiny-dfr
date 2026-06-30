use anyhow::{anyhow, Result};
use cairo::{Context, Surface};
use drm::control::ClipRect;

use crate::action::Action;
use crate::config::{ButtonConfig, Config};
use crate::pixel_shift::PIXEL_SHIFT_WIDTH_PX;
use crate::store::{StateKey, Store};
use crate::widgets::{Cell, Region, Slider, SliderBackend, Widget};
use crate::{dbg_ts, BUTTON_SPACING_PX};

#[derive(Clone, Copy)]
pub(crate) struct LayerArea {
    pub(crate) left: f64,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

impl LayerArea {
    pub(crate) fn full(width: i32, height: i32) -> LayerArea {
        LayerArea {
            left: 0.0,
            width,
            height,
        }
    }
}

#[derive(Default)]
pub struct FunctionLayer {
    displays_time: bool,
    pub(crate) cells: Vec<Cell>,
    virtual_button_count: usize,
    faster_refresh: bool,
}

impl FunctionLayer {
    pub(crate) fn with_config(cfg: Vec<ButtonConfig>) -> Result<FunctionLayer> {
        if cfg.is_empty() {
            return Err(anyhow!("layer has 0 buttons"));
        }

        let mut virtual_button_count = 0;
        let mut cells = Vec::with_capacity(cfg.len());
        for cfg in cfg {
            let mut stretch = cfg.stretch.unwrap_or(1);
            if stretch < 1 {
                println!("Stretch value must be at least 1, setting to 1.");
                stretch = 1;
            }
            virtual_button_count += stretch;
            cells.push(Cell {
                stretch,
                widget: Widget::from_config(cfg)?,
            });
        }
        let displays_time = cells.iter().any(|c| c.widget.is_clock());
        let faster_refresh = cells.iter().any(|c| c.widget.needs_faster_refresh());
        Ok(FunctionLayer {
            displays_time,
            cells,
            virtual_button_count,
            faster_refresh,
        })
    }
    /// A full-bar modal slider layer: one stretched `Slider` cell wrapping the
    /// given backend (brightness, keyboard illumination, volume, …).
    pub(crate) fn slider_layer(backend: Box<dyn SliderBackend>) -> FunctionLayer {
        FunctionLayer {
            displays_time: false,
            cells: vec![Cell {
                stretch: 1,
                widget: Widget::Slider(Slider::new(backend)),
            }],
            virtual_button_count: 1,
            faster_refresh: false,
        }
    }
    pub(crate) fn faster_refresh(&self) -> bool {
        self.faster_refresh
    }
    pub(crate) fn displays_time(&self) -> bool {
        self.displays_time
    }
    /// Whether anything in this layer needs repainting given the current state.
    pub(crate) fn needs_redraw(&self, store: &Store) -> bool {
        self.cells.iter().any(|c| c.widget.changed(store))
    }
    /// Press the interactive widget in cell `i`, returning its effect (if any).
    pub(crate) fn on_press_at(
        &mut self,
        i: usize,
        store: &Store,
        area: LayerArea,
        x: f64,
    ) -> Option<Action> {
        let region = self.region_for(area, i)?;
        match self.cells.get_mut(i).map(|c| &mut c.widget) {
            Some(Widget::Button(b)) => b.on_press(store),
            Some(Widget::Media(m)) => m.on_press(x, &region),
            _ => None,
        }
    }
    /// Release the interactive widget in cell `i`, returning its effect (if any).
    pub(crate) fn on_release_at(
        &mut self,
        i: usize,
        store: &Store,
        area: LayerArea,
    ) -> Option<Action> {
        let _region = self.region_for(area, i)?;
        match self.cells.get_mut(i).map(|c| &mut c.widget) {
            Some(Widget::Button(b)) => b.on_release(store),
            Some(Widget::Media(m)) => m.on_release(),
            _ => None,
        }
    }
    pub(crate) fn slider_key(&self) -> Result<StateKey> {
        self.cells
            .iter()
            .find_map(|cell| cell.widget.slider_key())
            .ok_or_else(|| anyhow!("layer has no slider"))
    }
    /// Anchor this layer's slider at the current touch.
    pub(crate) fn grab_slider(&mut self, level: f64, x: f64) {
        for cell in &mut self.cells {
            if let Widget::Slider(sl) = &mut cell.widget {
                sl.grab(x, level);
                return;
            }
        }
    }
    /// Feed a drag sample to this layer's slider, returning the backend effect.
    pub(crate) fn drag_slider(&mut self, x: f64, width: f64) -> Option<Action> {
        for cell in &mut self.cells {
            if let Widget::Slider(sl) = &mut cell.widget {
                let level = sl.drag(x, width);
                eprintln!("[dbg {:.6}] DRAG x={x:.0} level={level:.2}", dbg_ts());
                return Some(sl.backend.apply(level));
            }
        }
        None
    }
    /// Release this layer's slider (end of a drag).
    pub(crate) fn release_slider(&mut self) {
        for cell in &mut self.cells {
            if let Widget::Slider(sl) = &mut cell.widget {
                sl.release();
            }
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw(
        &mut self,
        config: &Config,
        width: i32,
        height: i32,
        surface: &Surface,
        pixel_shift: (f64, f64),
        store: &Store,
        complete_redraw: bool,
    ) -> Result<Vec<ClipRect>> {
        self.draw_in(
            config,
            LayerArea::full(width, height),
            surface,
            pixel_shift,
            store,
            complete_redraw,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_in(
        &mut self,
        config: &Config,
        area: LayerArea,
        surface: &Surface,
        pixel_shift: (f64, f64),
        store: &Store,
        complete_redraw: bool,
    ) -> Result<Vec<ClipRect>> {
        let c = Context::new(surface)?;
        let mut modified_regions = if complete_redraw {
            vec![ClipRect::new(
                0,
                area.left as u16,
                area.height as u16,
                (area.left + area.width as f64) as u16,
            )]
        } else {
            Vec::new()
        };
        c.translate(area.height as f64, 0.0);
        c.rotate((90.0f64).to_radians());
        let pixel_shift_width = if config.enable_pixel_shift {
            PIXEL_SHIFT_WIDTH_PX
        } else {
            0
        };
        let virtual_button_width = ((area.width - pixel_shift_width as i32)
            - (BUTTON_SPACING_PX * (self.virtual_button_count - 1) as i32))
            .max(1) as f64
            / self.virtual_button_count as f64;
        let (pixel_shift_x, pixel_shift_y) = pixel_shift;

        if complete_redraw {
            c.set_source_rgb(0.0, 0.0, 0.0);
            c.rectangle(area.left, 0.0, area.width as f64, area.height as f64);
            c.fill().unwrap();
        }
        c.set_font_face(&config.font_face);
        c.set_font_size(32.0);

        let mut start = 0;
        for cell in &mut self.cells {
            let end = start + cell.stretch;

            if !cell.widget.changed(store) && !complete_redraw {
                start = end;
                continue;
            };

            let left_edge = area.left
                + (start as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64)).floor()
                + pixel_shift_x
                + (pixel_shift_width / 2) as f64;

            let button_width = virtual_button_width
                + ((end - start - 1) as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64))
                    .floor();

            let region = Region {
                left: left_edge,
                width: button_width,
                height: area.height,
                y_shift: pixel_shift_y,
            };
            modified_regions.extend(cell.widget.draw(
                &c,
                config,
                &region,
                store,
                area.width,
                complete_redraw,
            )?);
            start = end;
        }

        Ok(modified_regions)
    }

    #[allow(dead_code)]
    pub(crate) fn hit(
        &self,
        width: u16,
        height: u16,
        x: f64,
        y: f64,
        i: Option<usize>,
    ) -> Option<usize> {
        self.hit_in(LayerArea::full(width as i32, height as i32), x, y, i)
    }

    fn region_for(&self, area: LayerArea, i: usize) -> Option<Region> {
        if i >= self.cells.len() {
            return None;
        }
        let virtual_button_width = (area.width
            - (BUTTON_SPACING_PX * (self.virtual_button_count - 1) as i32))
            .max(1) as f64
            / self.virtual_button_count as f64;
        let start: usize = self.cells[..i].iter().map(|c| c.stretch).sum();
        let end = start + self.cells[i].stretch;
        let left_edge =
            area.left + (start as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64)).floor();
        let button_width = virtual_button_width
            + ((end - start - 1) as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64))
                .floor();
        Some(Region {
            left: left_edge,
            width: button_width,
            height: area.height,
            y_shift: 0.0,
        })
    }

    pub(crate) fn hit_in(
        &self,
        area: LayerArea,
        x: f64,
        y: f64,
        i: Option<usize>,
    ) -> Option<usize> {
        if x < area.left || x > area.left + area.width as f64 {
            return None;
        }
        let local_x = x - area.left;
        let virtual_button_width = (area.width
            - (BUTTON_SPACING_PX * (self.virtual_button_count - 1) as i32))
            .max(1) as f64
            / self.virtual_button_count as f64;

        let i = i.unwrap_or_else(|| {
            let virtual_i =
                (local_x / (area.width as f64 / self.virtual_button_count as f64)) as usize;
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

        if local_x < left_edge
            || local_x > (left_edge + button_width)
            || y < 0.1 * area.height as f64
            || y > 0.9 * area.height as f64
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
