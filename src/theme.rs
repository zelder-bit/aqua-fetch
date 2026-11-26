//! Frutiger Aero look: sky gradient, glass panels, glossy buttons and bars.
use iced::widget::{button, container, progress_bar, scrollable, slider, text_editor};
use iced::{Background, Border, Color, Gradient, Shadow, Theme, Vector, border, gradient::Linear};
use std::f32::consts::PI;

pub const INK: Color = Color::from_rgb(0.04, 0.22, 0.36);
pub const INK_SOFT: Color = Color::from_rgb(0.16, 0.38, 0.52);
pub const WHITE: Color = Color::WHITE;

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgb(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0)
}

pub const AQUA: Color = rgb(0x0c, 0x8f, 0xd8);
pub const GREEN: Color = rgb(0x2e, 0xa0, 0x1c);
pub const SILVER: Color = rgb(0xc9, 0xd6, 0xe0);
pub const RED: Color = rgb(0xe2, 0x4b, 0x3c);
pub const AMBER: Color = rgb(0xf0, 0xb0, 0x2a);

fn mix(a: Color, b: Color, t: f32) -> Color {
    Color::from_rgba(a.r + (b.r - a.r) * t, a.g + (b.g - a.g) * t, a.b + (b.b - a.b) * t, a.a + (b.a - a.a) * t)
}

/// Top-to-bottom gradient (iced's angle 0 points up, so PI points down).
fn down(stops: &[(f32, Color)]) -> Background {
    let g = stops.iter().fold(Linear::new(PI), |g, (o, c)| g.add_stop(*o, *c));
    Background::Gradient(Gradient::Linear(g))
}

/// The classic gloss: a light top half, a hard line, then the base colour glowing back up.
fn gloss(base: Color) -> Background {
    down(&[
        (0.0, mix(base, WHITE, 0.6)),
        (0.48, mix(base, WHITE, 0.18)),
        (0.5, base),
        (1.0, mix(base, WHITE, 0.35)),
    ])
}

fn shadow(alpha: f32, blur: f32) -> Shadow {
    Shadow { color: Color::from_rgba(0.0, 0.2, 0.4, alpha), offset: Vector::new(0.0, 3.0), blur_radius: blur }
}

/// Window background used until the painted one is dropped in.
pub fn sky(_: &Theme) -> container::Style {
    container::Style {
        background: Some(down(&[
            (0.0, rgb(0x0a, 0x6f, 0xd6)),
            (0.45, rgb(0x4f, 0xc3, 0xf7)),
            (0.72, rgb(0xd9, 0xf6, 0xff)),
            (0.80, rgb(0xb6, 0xee, 0x8a)),
            (1.0, rgb(0x3f, 0xae, 0x2a)),
        ])),
        text_color: Some(INK),
        ..Default::default()
    }
}

pub fn glass(_: &Theme) -> container::Style {
    container::Style {
        background: Some(down(&[
            (0.0, Color::from_rgba(1.0, 1.0, 1.0, 0.78)),
            (0.12, Color::from_rgba(1.0, 1.0, 1.0, 0.50)),
            (1.0, Color::from_rgba(1.0, 1.0, 1.0, 0.36)),
        ])),
        border: border::rounded(16).color(Color::from_rgba(1.0, 1.0, 1.0, 0.9)).width(1.5),
        shadow: shadow(0.28, 18.0),
        text_color: Some(INK),
        ..Default::default()
    }
}

pub fn row_glass(_: &Theme) -> container::Style {
    container::Style {
        background: Some(down(&[
            (0.0, Color::from_rgba(1.0, 1.0, 1.0, 0.85)),
            (1.0, Color::from_rgba(0.90, 0.97, 1.0, 0.62)),
        ])),
        border: border::rounded(10).color(Color::from_rgba(1.0, 1.0, 1.0, 0.95)).width(1),
        shadow: shadow(0.12, 6.0),
        text_color: Some(INK),
        ..Default::default()
    }
}

pub fn glossy(base: Color) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let (c, alpha) = match status {
            button::Status::Active => (base, 1.0),
            button::Status::Hovered => (mix(base, WHITE, 0.18), 1.0),
            button::Status::Pressed => (mix(base, Color::BLACK, 0.15), 1.0),
            button::Status::Disabled => (mix(base, SILVER, 0.7), 0.6),
        };
        // Light buttons (silver) get dark text, coloured ones get white.
        let light = base.r + base.g + base.b > 1.7;
        button::Style {
            background: Some(gloss(c.scale_alpha(alpha))),
            text_color: if light { INK } else { WHITE }.scale_alpha(alpha),
            border: border::rounded(999).color(mix(c, Color::BLACK, 0.25).scale_alpha(alpha)).width(1),
            shadow: shadow(0.25 * alpha, 5.0),
            snap: false,
        }
    }
}

pub fn progress(base: Color) -> impl Fn(&Theme) -> progress_bar::Style {
    move |_| progress_bar::Style {
        background: down(&[(0.0, Color::from_rgba(0.0, 0.25, 0.45, 0.28)), (1.0, Color::from_rgba(1.0, 1.0, 1.0, 0.55))]),
        bar: gloss(base),
        border: border::rounded(999).color(Color::from_rgba(0.0, 0.25, 0.45, 0.35)).width(1),
    }
}

pub fn editor(_: &Theme, status: text_editor::Status) -> text_editor::Style {
    let focused = matches!(status, text_editor::Status::Focused { .. });
    text_editor::Style {
        background: Background::Color(Color::from_rgba(1.0, 1.0, 1.0, 0.82)),
        border: Border {
            color: if focused { AQUA } else { Color::from_rgba(0.0, 0.3, 0.5, 0.35) },
            width: if focused { 2.0 } else { 1.0 },
            radius: 10.into(),
        },
        placeholder: INK_SOFT.scale_alpha(0.7),
        value: INK,
        selection: AQUA.scale_alpha(0.35),
    }
}

pub fn knob(_: &Theme, _: slider::Status) -> slider::Style {
    slider::Style {
        rail: slider::Rail {
            backgrounds: (gloss(AQUA), Background::Color(Color::from_rgba(0.0, 0.25, 0.45, 0.25))),
            width: 8.0,
            border: border::rounded(999),
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Circle { radius: 9.0 },
            background: gloss(WHITE),
            border_width: 1.0,
            border_color: AQUA,
        },
    }
}

pub fn scroll(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let mut s = scrollable::default(theme, status);
    s.vertical_rail.background = Some(Background::Color(Color::from_rgba(1.0, 1.0, 1.0, 0.35)));
    s.vertical_rail.border = border::rounded(999);
    s.vertical_rail.scroller.background = gloss(AQUA);
    s.vertical_rail.scroller.border = border::rounded(999);
    s
}
