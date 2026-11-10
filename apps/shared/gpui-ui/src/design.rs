use crate::icons::{icon, Icon};
use gpui::{
    colors::Colors, div, img, prelude::*, px, rgba, FontWeight, Image, ImageFormat, Rgba,
    SharedString, Window, WindowAppearance,
};
use std::{ops::Deref, sync::Arc};

pub const FONT_META: f32 = 12.;
pub const FONT_ACTION: f32 = 13.;
pub const FONT_BODY: f32 = 14.;
pub const FONT_DISPLAY: f32 = 54.;
pub const WEIGHT_CAPTION: FontWeight = FontWeight(450.);
pub const WEIGHT_OPTION: FontWeight = FontWeight(475.);
pub const ICON_BUTTON: f32 = 26.;
pub const NAV_ROW: f32 = 32.;
pub const SELECTOR_CHIP: f32 = 28.;
pub const SESSION_INDENT: f32 = 39.;
pub const CONTENT_WIDTH: f32 = 760.;
pub const COMPOSER_RADIUS: f32 = 32.;

#[derive(Clone)]
pub struct Design {
    pub colors: Colors,
    pub sidebar: Rgba,
    pub menu: Rgba,
    pub hover: Rgba,
    pub muted: Rgba,
    pub faint: Rgba,
    pub accent: Rgba,
}

impl Deref for Design {
    type Target = Colors;

    fn deref(&self) -> &Self::Target {
        &self.colors
    }
}

impl Design {
    pub fn for_window(window: &Window) -> Self {
        if matches!(
            window.appearance(),
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        ) {
            Self {
                colors: Colors {
                    text: rgba(0xffffffd6),
                    selected_text: rgba(0xffffffd6),
                    background: rgba(0x121212ff),
                    disabled: rgba(0xffffff6b),
                    selected: rgba(0xffffff19),
                    border: rgba(0xffffff1f),
                    separator: rgba(0xffffff2e),
                    container: rgba(0x1f1f1fff),
                },
                sidebar: rgba(0x0d0d0dff),
                menu: rgba(0x292929f2),
                hover: rgba(0xffffff0d),
                muted: rgba(0xffffff8f),
                faint: rgba(0xffffff6b),
                accent: rgba(0x1a88ffff),
            }
        } else {
            Self {
                colors: Colors {
                    text: rgba(0x000000e6),
                    selected_text: rgba(0x000000e6),
                    background: rgba(0xffffffff),
                    disabled: rgba(0x00000073),
                    selected: rgba(0x0000000d),
                    border: rgba(0x00000021),
                    separator: rgba(0x00000033),
                    container: rgba(0xffffffff),
                },
                sidebar: rgba(0xf9fbfcff),
                menu: rgba(0xfffffff2),
                hover: rgba(0x00000008),
                muted: rgba(0x00000099),
                faint: rgba(0x00000073),
                accent: rgba(0x1783ffff),
            }
        }
    }
}

fn ascii_logo(bytes: &[u8], width: f32) -> gpui::Img {
    let logo = Image::from_bytes(ImageFormat::Svg, bytes.to_vec());
    img(Arc::new(logo)).w(px(width))
}

pub fn brand_mark() -> gpui::Img {
    ascii_logo(include_bytes!("../../../../docs/noel-ascii-mark.svg"), 84.)
}

pub fn home_mark() -> gpui::Img {
    ascii_logo(include_bytes!("../../../../docs/noel-ascii-logo.svg"), 304.)
}

pub fn folder_icon(design: &Design) -> gpui::Svg {
    icon(Icon::Folder, 16., design.muted)
}

pub fn icon_button(
    id: impl Into<SharedString>,
    glyph: Icon,
    design: &Design,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .w(px(ICON_BUTTON))
        .h(px(ICON_BUTTON))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_size(px(FONT_ACTION))
        .text_color(design.muted)
        .cursor_pointer()
        .hover(|button| button.bg(design.hover).text_color(design.text))
        .child(icon(glyph, 14., design.muted))
}

pub fn sidebar_action(
    id: impl Into<SharedString>,
    glyph: Icon,
    label: &str,
    design: &Design,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .h(px(NAV_ROW))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .rounded_md()
        .text_size(px(FONT_ACTION))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .hover(|button| button.bg(design.hover))
        .child(icon(glyph, 16., design.text))
        .child(label.to_owned())
}

pub fn menu_action(
    id: impl Into<SharedString>,
    glyph: Icon,
    label: &str,
    design: &Design,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .h(px(30.))
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .rounded_md()
        .text_size(px(FONT_ACTION))
        .cursor_pointer()
        .hover(|button| button.bg(design.hover))
        .child(icon(glyph, 14., design.muted))
        .child(label.to_owned())
}
