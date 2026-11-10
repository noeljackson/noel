use gpui::{prelude::*, px, svg, Rgba};

#[derive(Clone, Copy)]
pub enum Icon {
    Archive,
    ArrowUp,
    Check,
    ChevronDown,
    ChevronUp,
    CirclePlus,
    Code,
    Copy,
    Folder,
    FolderPlus,
    More,
    Plus,
    Search,
    Trash,
    X,
}

const HEADER: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">"#;

fn body(icon: Icon) -> &'static str {
    match icon {
        Icon::Archive => {
            r#"<rect width="20" height="5" x="2" y="3" rx="1"/><path d="M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8"/><path d="M10 12h4"/>"#
        }
        Icon::ArrowUp => r#"<path d="m5 12 7-7 7 7"/><path d="M12 19V5"/>"#,
        Icon::Check => r#"<path d="M20 6 9 17l-5-5"/>"#,
        Icon::ChevronDown => r#"<path d="m6 9 6 6 6-6"/>"#,
        Icon::ChevronUp => r#"<path d="m6 15 6-6 6 6"/>"#,
        Icon::CirclePlus => {
            r#"<circle cx="12" cy="12" r="10"/><path d="M8 12h8"/><path d="M12 8v8"/>"#
        }
        Icon::Code => r#"<path d="m16 18 6-6-6-6"/><path d="m8 6-6 6 6 6"/>"#,
        Icon::Copy => {
            r#"<rect width="14" height="14" x="8" y="8" rx="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/>"#
        }
        Icon::Folder => {
            r#"<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>"#
        }
        Icon::FolderPlus => {
            r#"<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/><path d="M12 10v6"/><path d="M9 13h6"/>"#
        }
        Icon::More => {
            r#"<circle cx="5" cy="12" r="1" fill="currentColor" stroke="none"/><circle cx="12" cy="12" r="1" fill="currentColor" stroke="none"/><circle cx="19" cy="12" r="1" fill="currentColor" stroke="none"/>"#
        }
        Icon::Plus => r#"<path d="M5 12h14"/><path d="M12 5v14"/>"#,
        Icon::Search => r#"<circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/>"#,
        Icon::Trash => {
            r#"<path d="M3 6h18"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/><path d="M10 11v6"/><path d="M14 11v6"/>"#
        }
        Icon::X => r#"<path d="M18 6 6 18"/><path d="m6 6 12 12"/>"#,
    }
}

pub fn icon(icon: Icon, size: f32, color: Rgba) -> gpui::Svg {
    let data = format!("{HEADER}{}</svg>", body(icon));
    svg().data(data.as_bytes()).size(px(size)).text_color(color)
}
