//! What floats: tooltip, popover, context menu, modal, toast. Each is `overlay` fill, a 1 pt
//! edge and a shadow; panes never cast one. No blur.

use super::glyph;
use crate::gui::theme::{self, Type};
use gpui::{
    div, prelude::*, px, Animation, AnimationElement, AnimationExt, AnyView, App, Context, Div, ElementId, Rgba,
    SharedString, Stateful, Window,
};
use std::time::Duration;

struct Tip {
    words: SharedString,
    shortcut: Option<SharedString>,
}

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(240.))
            .px(px(8.))
            .py(px(4.))
            .rounded(px(theme::R_CONTROL))
            .bg(theme::tooltip())
            .border_1()
            .border_color(theme::border())
            .shadow(theme::shadow_tooltip())
            .flex()
            .gap(px(8.))
            .font_family(theme::FONT)
            .type_small()
            .text_color(theme::text())
            .child(div().flex_1().min_w_0().whitespace_normal().overflow_hidden().child(self.words.clone()))
            .children(self.shortcut.clone().map(|keys| div().flex_none().whitespace_nowrap().text_color(theme::text_faint()).child(keys)))
    }
}

/// For `.tooltip(..)`: the words, and the shortcut that does the same.
pub fn tip(
    words: impl Into<SharedString>,
    shortcut: Option<&'static str>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let words: SharedString = words.into();
    let words: SharedString = crate::gui::shortcuts::label(&words).into();
    move |_, cx| {
        let words = words.clone();
        cx.new(|_| Tip { words, shortcut: shortcut.map(|keys| crate::gui::shortcuts::label(keys).into()) }).into()
    }
}

fn floating(radius: f32) -> Div {
    div()
        .rounded(px(radius))
        .bg(theme::overlay())
        .border_1()
        .border_color(theme::overlay_edge())
        .font_family(theme::FONT)
        .type_ui()
        .text_color(theme::text())
        // A floating surface takes the pointer: nothing under it hovers or clicks.
        .occlude()
}

/// An elevated panel, 200 to 360 wide, placed 6 from its anchor by the caller.
pub fn popover() -> Div {
    floating(theme::R_FLOAT).p(px(12.)).min_w(px(200.)).max_w(px(360.)).shadow(theme::shadow_popover())
}

/// A context menu: a column of [`menu_item`]s, [`menu_separator`]s and [`menu_label`]s.
pub fn menu() -> Div {
    floating(theme::R_FLOAT)
        .p(px(4.))
        .min_w(px(180.))
        .max_w(px(280.))
        .shadow(theme::shadow_popover())
        .flex()
        .flex_col()
}

/// Pass as a menu item's glyph to keep the 16 pt leading slot empty, so labels line up in a menu
/// where any item has a glyph.
pub const NO_GLYPH: &str = "";

fn menu_row(
    id: impl Into<ElementId>,
    icon: Option<&'static str>,
    label: impl Into<SharedString>,
    shortcut: Option<&'static str>,
    color: Rgba,
    icon_color: Rgba,
    highlight: Rgba,
) -> Stateful<Div> {
    let lead = icon.map(|icon| match icon {
        NO_GLYPH => div().flex_none().size(px(16.)).into_any_element(),
        icon => glyph(icon, 16., icon_color).into_any_element(),
    });
    div()
        .id(id)
        .group("menu-item")
        .flex_none()
        .h(px(theme::row_height(theme::ROW_ONE)))
        .px(px(8.))
        .rounded(px(theme::R_CONTROL))
        .flex()
        .items_center()
        .gap(px(8.))
        .cursor_pointer()
        .text_color(color)
        .hover(move |style| style.bg(highlight))
        .focus_visible(|style| style.shadow(theme::focus_ring_inset()))
        .children(lead)
        .child(div().flex_1().min_w_0().truncate().child(label.into()))
        .children(shortcut.map(|keys| {
            div()
                .flex_none()
                .type_small()
                .text_color(theme::text_faint())
                .group_hover("menu-item", |style| style.text_color(theme::text_soft()))
                .child(crate::gui::shortcuts::label(keys))
        }))
}

/// A choice in a menu: the current one carries a check in the leading slot. No radio circles,
/// no fills.
pub fn menu_item_check(
    id: impl Into<ElementId>,
    checked: bool,
    label: impl Into<SharedString>,
    shortcut: Option<&'static str>,
) -> Stateful<Div> {
    menu_item(id, Some(if checked { "icons/check.svg" } else { NO_GLYPH }), label, shortcut)
}

/// An item that does not apply here: drawn, never hidden, and it takes no click.
pub fn menu_item_disabled(
    id: impl Into<ElementId>,
    icon: Option<&'static str>,
    label: impl Into<SharedString>,
    shortcut: Option<&'static str>,
) -> Stateful<Div> {
    menu_row(id, icon, label, shortcut, theme::text_disabled(), theme::text_disabled(), gpui::rgba(0)).cursor_default()
}

/// The highlight is `selected`, never accent.
pub fn menu_item(
    id: impl Into<ElementId>,
    icon: Option<&'static str>,
    label: impl Into<SharedString>,
    shortcut: Option<&'static str>,
) -> Stateful<Div> {
    menu_row(id, icon, label, shortcut, theme::text(), theme::text_soft(), theme::selected())
}

pub fn menu_item_danger(
    id: impl Into<ElementId>,
    icon: Option<&'static str>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    menu_row(id, icon, label, None, theme::danger(), theme::danger(), theme::danger_soft())
}

pub fn menu_separator() -> Div {
    div().flex_none().my(px(4.)).h(px(1.)).bg(theme::hairline())
}

/// A menu's own label: 24 high, 11/500 `text_faint`, sentence case.
pub fn menu_label(label: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .h(px(theme::MENU_LABEL))
        .px(px(8.))
        .flex()
        .items_center()
        .type_micro()
        .text_color(theme::text_faint())
        .child(label.into())
}

/// What dims the window behind a modal. It takes every click; the caller closes on one.
pub fn scrim() -> Div {
    div().absolute().top_0().left_0().size_full().bg(theme::scrim()).occlude()
}

/// A modal panel: 440 wide, title, then the caller's body and a [`modal_buttons`] row.
pub fn modal(title: impl Into<SharedString>) -> Div {
    floating(theme::R_MODAL)
        .w(px(440.))
        .p(px(20.))
        .shadow(theme::shadow_modal())
        .flex()
        .flex_col()
        .child(div().type_title().child(title.into()))
}

/// A modal's words: 13 / 18 in `text_soft`.
#[allow(dead_code)]
pub fn modal_body(words: impl Into<SharedString>) -> Div {
    div().mt(px(8.)).type_ui().text_color(theme::text_soft()).child(words.into())
}

/// Buttons sit right, 8 apart, the main action last.
#[allow(dead_code)]
pub fn modal_buttons() -> Div {
    div().mt(px(20.)).flex().justify_end().gap(px(8.))
}

/// A sheet: the same panel hung from the title bar, so its top corners are square.
#[allow(dead_code)]
pub fn sheet(title: impl Into<SharedString>) -> Div {
    modal(title).rounded_t(px(0.))
}

/// One toast: an optional glyph in its semantic color, the words, and the caller's ghost action.
/// 340 wide; the window's layer narrows it to `place::toast_width` in a small window.
pub fn toast(icon: Option<(&'static str, Rgba)>, words: impl Into<SharedString>) -> Div {
    floating(theme::R_FLOAT)
        .w(px(340.))
        .min_h(px(40.))
        .p(px(12.))
        .shadow(theme::shadow_popover())
        .flex()
        .items_center()
        .gap(px(8.))
        .children(icon.map(|(icon, color)| glyph(icon, 16., color)))
        .child(div().flex_1().min_w_0().child(words.into()))
}

/// Fades a floating element in as it mounts: menus and popovers 120 ms, modals and toasts 160.
/// It runs once and asks for no frame afterwards. Under Reduce Motion it is drawn whole at once.
pub fn fade_in<E: IntoElement + Styled + 'static>(
    id: impl Into<ElementId>,
    milliseconds: u64,
    element: E,
) -> AnimationElement<E> {
    let milliseconds = if crate::gui::app::reduce_motion() { 0 } else { milliseconds };
    let animation = Animation::new(Duration::from_millis(milliseconds.max(1))).with_easing(gpui::ease_out_quint());
    element.with_animation(id, animation, |element, delta| element.opacity(delta))
}
