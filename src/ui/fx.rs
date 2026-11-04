//! Animaciones (tachyonfx). Se piden con banderas desde la lógica y se crean
//! durante el dibujo, cuando ya se conocen las áreas.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tachyonfx::{EffectManager, Interpolation, Motion, fx};

use super::theme::{ACCENT_BG, ACCENT_SOFT, BG, SURFACE};

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum FxKey {
    #[default]
    Intro,
    Screen,
    Modal,
    Flash,
    Toast,
}

pub struct Fx {
    mgr: EffectManager<FxKey>,
    pub enabled: bool,
    pub intro: bool,
    pub screen: Option<Motion>,
    pub modal: bool,
    pub flash: bool,
    pub toast: bool,
    shimmer_start: Option<Instant>,
    last: Instant,
}

const SHIMMER: Duration = Duration::from_millis(1400);

impl Fx {
    pub fn new(enabled: bool) -> Fx {
        Fx {
            mgr: EffectManager::default(),
            enabled,
            intro: enabled,
            screen: None,
            modal: false,
            flash: false,
            toast: false,
            shimmer_start: enabled.then(Instant::now),
            last: Instant::now(),
        }
    }

    pub fn shimmer(&mut self) {
        if self.enabled {
            self.shimmer_start = Some(Instant::now());
        }
    }

    /// Progreso del brillo del logo (0..1) o `None` si no corre.
    pub fn shimmer_phase(&self) -> Option<f32> {
        let s = self.shimmer_start?;
        let t = s.elapsed().as_secs_f32() / SHIMMER.as_secs_f32();
        (t < 1.0).then_some(t)
    }

    pub fn running(&self) -> bool {
        self.mgr.is_running() || self.shimmer_phase().is_some()
    }

    pub fn intro(&mut self, area: Rect) {
        if std::mem::take(&mut self.intro) && self.enabled {
            let e = fx::parallel(&[
                fx::fade_from(BG, BG, (650, Interpolation::QuadOut)),
                fx::sweep_in(Motion::LeftToRight, 36, 18, BG, (750, Interpolation::QuadOut)),
            ]);
            self.mgr.add_unique_effect(FxKey::Intro, e.with_area(area));
        }
    }

    pub fn screen(&mut self, area: Rect) {
        if let Some(m) = self.screen.take()
            && self.enabled {
                let e = fx::sweep_in(m, 28, 0, BG, (320, Interpolation::CubicOut));
                self.mgr.add_unique_effect(FxKey::Screen, e.with_area(area));
            }
    }

    pub fn modal(&mut self, area: Rect) {
        if std::mem::take(&mut self.modal) && self.enabled {
            let e = fx::parallel(&[
                fx::fade_from(SURFACE, SURFACE, (220, Interpolation::QuadOut)),
                fx::coalesce((300, Interpolation::ExpoOut)),
            ]);
            self.mgr.add_unique_effect(FxKey::Modal, e.with_area(area));
        }
    }

    pub fn flash(&mut self, area: Option<Rect>) {
        if std::mem::take(&mut self.flash) && self.enabled
            && let Some(a) = area {
                let e = fx::fade_from(ACCENT_SOFT, ACCENT_BG, (700, Interpolation::QuadOut));
                self.mgr.add_unique_effect(FxKey::Flash, e.with_area(a));
            }
    }

    pub fn toast(&mut self, area: Rect) {
        if std::mem::take(&mut self.toast) && self.enabled {
            let e = fx::fade_from(BG, BG, (260, Interpolation::QuadOut));
            self.mgr.add_unique_effect(FxKey::Toast, e.with_area(area));
        }
    }

    pub fn process(&mut self, buf: &mut Buffer, area: Rect) {
        let now = Instant::now();
        let dt = now - self.last;
        self.last = now;
        if self.enabled {
            self.mgr.process_effects(dt.into(), buf, area);
        }
    }
}
