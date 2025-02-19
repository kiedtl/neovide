mod blink;
mod cursor_vfx;

use std::collections::HashMap;

// use neovide_derive::SettingGroup;
use skulpin::skia_safe::{Canvas, Paint, Path, Point};

use super::RenderedWindow;
use crate::bridge::EditorMode;
use crate::editor::{Colors, Cursor, CursorShape};
use crate::redraw_scheduler::REDRAW_SCHEDULER;
use crate::renderer::animation_utils::*;
use crate::renderer::CachingShaper;
use crate::settings::{FromValue, SETTINGS};

use blink::*;

const DEFAULT_CELL_PERCENTAGE: f32 = 1.0 / 8.0;

const STANDARD_CORNERS: &[(f32, f32); 4] = &[(-0.5, -0.5), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)];

#[derive(Clone, SettingGroup)]
#[setting_prefix = "cursor"]
pub struct CursorSettings {
    antialiasing: bool,
    animation_length: f32,
    distance_length_adjust: bool,
    animate_in_insert_mode: bool,
    animate_command_line: bool,
    trail_size: f32,
    vfx_mode: cursor_vfx::VfxMode,
    vfx_opacity: f32,
    vfx_particle_lifetime: f32,
    vfx_particle_density: f32,
    vfx_particle_speed: f32,
    vfx_particle_phase: f32,
    vfx_particle_curl: f32,
}

impl Default for CursorSettings {
    fn default() -> Self {
        CursorSettings {
            antialiasing: true,
            animation_length: 0.13,
            animate_in_insert_mode: true,
            animate_command_line: true,
            trail_size: 0.7,
            distance_length_adjust: false,
            vfx_mode: cursor_vfx::VfxMode::Disabled,
            vfx_opacity: 200.0,
            vfx_particle_lifetime: 1.2,
            vfx_particle_density: 7.0,
            vfx_particle_speed: 10.0,
            vfx_particle_phase: 1.5,
            vfx_particle_curl: 1.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Corner {
    start_position: Point,
    current_position: Point,
    relative_position: Point,
    previous_destination: Point,
    length_multiplier: f32,
    t: f32,
}

impl Corner {
    pub fn new() -> Corner {
        Corner {
            start_position: Point::new(0.0, 0.0),
            current_position: Point::new(0.0, 0.0),
            relative_position: Point::new(0.0, 0.0),
            previous_destination: Point::new(-1000.0, -1000.0),
            length_multiplier: 1.0,
            t: 0.0,
        }
    }

    pub fn update(
        &mut self,
        settings: &CursorSettings,
        font_dimensions: Point,
        destination: Point,
        dt: f32,
        immediate_movement: bool,
    ) -> bool {
        if destination != self.previous_destination {
            self.t = 0.0;
            self.start_position = self.current_position;
            self.previous_destination = destination;
            self.length_multiplier = if settings.distance_length_adjust {
                (destination - self.current_position).length().log10()
            } else {
                1.0
            }
        }

        // Check first if animation's over
        if (self.t - 1.0).abs() < std::f32::EPSILON {
            return false;
        }

        // Calculate window-space destination for corner
        let relative_scaled_position: Point = (
            self.relative_position.x * font_dimensions.x,
            self.relative_position.y * font_dimensions.y,
        )
            .into();

        let corner_destination = destination + relative_scaled_position;

        if immediate_movement {
            self.t = 1.0;
            self.current_position = corner_destination;
            return true;
        }

        // Calculate how much a corner will be lagging behind based on how much it's aligned
        // with the direction of motion. Corners in front will move faster than corners in the
        // back
        let travel_direction = {
            let mut d = destination - self.current_position;
            d.normalize();
            d
        };

        let corner_direction = {
            let mut d = self.relative_position;
            d.normalize();
            d
        };

        let direction_alignment = travel_direction.dot(corner_direction);

        if (self.t - 1.0).abs() < std::f32::EPSILON {
            // We are at destination, move t out of 0-1 range to stop the animation
            self.t = 2.0;
        } else {
            let corner_dt = dt
                * lerp(
                    1.0,
                    (1.0 - settings.trail_size).max(0.0).min(1.0),
                    -direction_alignment,
                );
            self.t = (self.t + corner_dt / (settings.animation_length * self.length_multiplier)).min(1.0)
        }

        self.current_position = ease_point(
            ease_out_expo,
            self.start_position,
            corner_destination,
            self.t,
        );

        true
    }
}

pub struct CursorRenderer {
    pub corners: Vec<Corner>,
    cursor: Cursor,
    destination: Point,
    blink_status: BlinkStatus,
    previous_cursor_shape: Option<CursorShape>,
    previous_editor_mode: EditorMode,
    cursor_vfx: Option<Box<dyn cursor_vfx::CursorVfx>>,
    previous_vfx_mode: cursor_vfx::VfxMode,
}

impl CursorRenderer {
    pub fn new() -> CursorRenderer {
        let mut renderer = CursorRenderer {
            corners: vec![Corner::new(); 4],
            cursor: Cursor::new(),
            destination: (0.0, 0.0).into(),
            blink_status: BlinkStatus::new(),
            previous_cursor_shape: None,
            previous_editor_mode: EditorMode::Normal,
            cursor_vfx: None,
            previous_vfx_mode: cursor_vfx::VfxMode::Disabled,
        };
        renderer.set_cursor_shape(&CursorShape::Block, DEFAULT_CELL_PERCENTAGE);
        renderer
    }

    pub fn update_cursor(&mut self, new_cursor: Cursor) {
        self.cursor = new_cursor;
    }

    fn set_cursor_shape(&mut self, cursor_shape: &CursorShape, cell_percentage: f32) {
        self.corners = self
            .corners
            .clone()
            .into_iter()
            .enumerate()
            .map(|(i, corner)| {
                let (x, y) = STANDARD_CORNERS[i];

                Corner {
                    relative_position: match cursor_shape {
                        CursorShape::Block => (x, y).into(),
                        // Transform the x position so that the right side is translated over to
                        // the BAR_WIDTH position
                        CursorShape::Vertical => ((x + 0.5) * cell_percentage - 0.5, y).into(),
                        // Do the same as above, but flip the y coordinate and then flip the result
                        // so that the horizontal bar is at the bottom of the character space
                        // instead of the top.
                        CursorShape::Horizontal => {
                            (x, -((-y + 0.5) * cell_percentage - 0.5)).into()
                        }
                    },
                    t: 0.0,
                    start_position: corner.current_position,
                    ..corner
                }
            })
            .collect::<Vec<Corner>>();
    }

    pub fn update_cursor_destination(
        &mut self,
        font_width: f32,
        font_height: f32,
        windows: &HashMap<u64, RenderedWindow>,
        current_mode: &EditorMode,
    ) {
        let (cursor_grid_x, cursor_grid_y) = self.cursor.grid_position;

        if let Some(window) = windows.get(&self.cursor.parent_window_id) {
            if cursor_grid_y < window.grid_height-1 || matches!(current_mode, EditorMode::CmdLine) {
                let grid_x = cursor_grid_x as f32 + window.grid_current_position.x;
                let mut grid_y = cursor_grid_y as f32 + window.grid_current_position.y
                    - (window.current_scroll - window.current_surfaces.top_line);

                // Prevent the cursor from targeting a position outside its current window. Since only
                // the vertical direction is effected by scrolling, we only have to clamp the vertical
                // grid position.
                grid_y = grid_y
                    .max(window.grid_current_position.y)
                    .min(window.grid_current_position.y + window.grid_height as f32 - 1.0);

                self.destination = (grid_x * font_width, grid_y * font_height).into();
            }
        } else {
            self.destination = (
                cursor_grid_x as f32 * font_width,
                cursor_grid_y as f32 * font_height,
            )
                .into();
        }
    }

    pub fn draw(
        &mut self,
        default_colors: &Colors,
        font_size: (f32, f32),
        current_mode: &EditorMode,
        shaper: &mut CachingShaper,
        canvas: &mut Canvas,
        dt: f32,
    ) {
        let (font_width, font_height) = font_size;
        let render = self.blink_status.update_status(&self.cursor);
        let settings = SETTINGS.get::<CursorSettings>();

        if settings.vfx_mode != self.previous_vfx_mode {
            self.cursor_vfx = cursor_vfx::new_cursor_vfx(&settings.vfx_mode);
            self.previous_vfx_mode = settings.vfx_mode.clone();
        }

        let mut paint = Paint::new(skulpin::skia_safe::colors::WHITE, None);
        paint.set_anti_alias(settings.antialiasing);

        let character = self.cursor.character.clone();

        let font_width = match (self.cursor.double_width, &self.cursor.shape) {
            (true, CursorShape::Block) => font_width * 2.0,
            _ => font_width,
        };

        let font_dimensions: Point = (font_width, font_height).into();

        let in_insert_mode = matches!(current_mode, EditorMode::Insert);
        let changed_to_from_cmdline = !matches!(self.previous_editor_mode, EditorMode::CmdLine) 
                                      ^ matches!(current_mode, EditorMode::CmdLine);

        let center_destination = self.destination + font_dimensions * 0.5;
        let new_cursor = Some(self.cursor.shape.clone());

        if self.previous_cursor_shape != new_cursor {
            self.previous_cursor_shape = new_cursor.clone();
            self.set_cursor_shape(
                &new_cursor.unwrap(),
                self.cursor
                    .cell_percentage
                    .unwrap_or(DEFAULT_CELL_PERCENTAGE),
            );

            if let Some(vfx) = self.cursor_vfx.as_mut() {
                vfx.restart(center_destination);
            }
        }

        let mut animating = false;

        if !center_destination.is_zero() {
            for corner in self.corners.iter_mut() {
                let corner_animating = corner.update(
                    &settings,
                    font_dimensions,
                    center_destination,
                    dt,
                    !settings.animate_in_insert_mode && in_insert_mode 
                    || !settings.animate_command_line && !changed_to_from_cmdline,
                );

                animating |= corner_animating;
            }

            let vfx_animating = if let Some(vfx) = self.cursor_vfx.as_mut() {
                vfx.update(&settings, center_destination, (font_width, font_height), dt)
            } else {
                false
            };

            animating |= vfx_animating;
        }

        if animating {
            REDRAW_SCHEDULER.queue_next_frame();
        } else {
            self.previous_editor_mode = current_mode.clone();
        }

        if self.cursor.enabled && render {
            // Draw Background
            paint.set_color(self.cursor.background(&default_colors).to_color());

            // The cursor is made up of four points, so I create a path with each of the four
            // corners.
            let mut path = Path::new();

            path.move_to(self.corners[0].current_position);
            path.line_to(self.corners[1].current_position);
            path.line_to(self.corners[2].current_position);
            path.line_to(self.corners[3].current_position);
            path.close();

            canvas.draw_path(&path, &paint);

            // Draw foreground
            paint.set_color(self.cursor.foreground(&default_colors).to_color());

            canvas.save();
            canvas.clip_path(&path, None, Some(false));

            let blobs = &shaper.shape_cached(&character, false, false);

            for blob in blobs.iter() {
                canvas.draw_text_blob(&blob, self.destination, &paint);
            }

            canvas.restore();

            if let Some(vfx) = self.cursor_vfx.as_ref() {
                vfx.render(
                    &settings,
                    canvas,
                    &self.cursor,
                    &default_colors,
                    (font_width, font_height),
                );
            }
        }
    }
}
