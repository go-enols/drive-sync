//! PolyBot 风格主题令牌：深色 / 浅色两套 palette，全部 UI 只使用主题令牌。
//!
//! 深色：bg `#0B0F14`、card `#111820`、border `#1E2A33`、主文字 `#E6EDF3`、次要 `#9CA3AF`、
//! 主色 `#22E3B0`、危险 `#F87171`；
//! 浅色：bg `#F5F7FA`、card `#FFFFFF`、border `#E2E8F0`、主文字 `#0B1220`、次要 `#64748B`、
//! 主色 `#0EA37F`（保证白底对比度）、危险 `#DC2626`。
#![allow(dead_code)]

use std::sync::{OnceLock, RwLock};

use egui::{
    Align2, Color32, CornerRadius, FontId, Frame, Margin, RichText, Sense, Stroke, Ui, Vec2,
};
use serde::{Deserialize, Serialize};

pub const CARD_RADIUS: u8 = 12;
pub const SMALL_RADIUS: u8 = 8;

/// 一套完整主题调色板。
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub light: bool,
    pub bg: Color32,
    pub panel: Color32,
    pub panel_dark: Color32,
    pub panel_hover: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub mint: Color32,
    pub mint_dark: Color32,
    pub danger: Color32,
    pub warn: Color32,
    pub grid: Color32,
    /// 文本选择时的前景色。
    pub selection_fg: Color32,
}

pub const DARK: Palette = Palette {
    light: false,
    bg: Color32::from_rgb(0x0B, 0x0F, 0x14),
    panel: Color32::from_rgb(0x11, 0x18, 0x20),
    panel_dark: Color32::from_rgb(0x0E, 0x14, 0x1A),
    panel_hover: Color32::from_rgb(0x16, 0x22, 0x2B),
    border: Color32::from_rgb(0x1E, 0x2A, 0x33),
    text: Color32::from_rgb(0xE6, 0xED, 0xF3),
    muted: Color32::from_rgb(0x9C, 0xA3, 0xAF),
    mint: Color32::from_rgb(0x22, 0xE3, 0xB0),
    mint_dark: Color32::from_rgb(0x08, 0x11, 0x0C),
    danger: Color32::from_rgb(0xF8, 0x71, 0x71),
    warn: Color32::from_rgb(0xFB, 0xBF, 0x24),
    grid: Color32::from_rgb(0x0F, 0x16, 0x1D),
    selection_fg: Color32::from_rgb(0x08, 0x11, 0x0C),
};

pub const LIGHT: Palette = Palette {
    light: true,
    bg: Color32::from_rgb(0xF5, 0xF7, 0xFA),
    panel: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    panel_dark: Color32::from_rgb(0xF1, 0xF5, 0xF9),
    panel_hover: Color32::from_rgb(0xEA, 0xEF, 0xF5),
    border: Color32::from_rgb(0xE2, 0xE8, 0xF0),
    text: Color32::from_rgb(0x0B, 0x12, 0x20),
    muted: Color32::from_rgb(0x64, 0x74, 0x8B),
    mint: Color32::from_rgb(0x0E, 0xA3, 0x7F),
    mint_dark: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    danger: Color32::from_rgb(0xDC, 0x26, 0x26),
    warn: Color32::from_rgb(0xB4, 0x53, 0x09),
    grid: Color32::from_rgb(0xEB, 0xF0, 0xF6),
    selection_fg: Color32::from_rgb(0xFF, 0xFF, 0xFF),
};

/// 外观主题模式（持久化到 `<data-dir>/gui.json`）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    /// 跟随系统（默认）。
    #[default]
    System,
    Dark,
    Light,
}

impl ThemeMode {
    pub const ALL: [ThemeMode; 3] = [ThemeMode::System, ThemeMode::Dark, ThemeMode::Light];

    pub fn label(self) -> &'static str {
        match self {
            ThemeMode::System => "跟随系统",
            ThemeMode::Dark => "深色",
            ThemeMode::Light => "浅色",
        }
    }

    /// 解析为实际使用的 palette（跟随系统失败时回退深色）。
    pub fn palette(self, system_light: Option<bool>) -> &'static Palette {
        match self {
            ThemeMode::Dark => &DARK,
            ThemeMode::Light => &LIGHT,
            ThemeMode::System => {
                if system_light.unwrap_or(false) {
                    &LIGHT
                } else {
                    &DARK
                }
            }
        }
    }
}

/// GUI 专用持久化设置（与 CLI 的 config.json 完全分离）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GuiSettings {
    #[serde(default)]
    pub theme: ThemeMode,
}

pub fn gui_settings_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("gui.json")
}

pub fn load_gui_settings(data_dir: &std::path::Path) -> GuiSettings {
    let path = gui_settings_path(data_dir);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return GuiSettings::default();
    };
    serde_json::from_str(&raw).unwrap_or_else(|error| {
        tracing::warn!(path = %path.display(), error = %error, "gui.json 解析失败，使用默认主题");
        GuiSettings::default()
    })
}

pub fn save_gui_settings(data_dir: &std::path::Path, settings: &GuiSettings) -> anyhow::Result<()> {
    let path = gui_settings_path(data_dir);
    let bytes = serde_json::to_vec_pretty(settings)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

static ACTIVE: OnceLock<RwLock<&'static Palette>> = OnceLock::new();

fn active_cell() -> &'static RwLock<&'static Palette> {
    ACTIVE.get_or_init(|| RwLock::new(&DARK))
}

/// 当前生效 palette。
pub fn current() -> &'static Palette {
    *active_cell()
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// 切换当前 palette；返回是否发生变化。
pub fn set_active(palette: &'static Palette) -> bool {
    let mut active = active_cell()
        .write()
        .unwrap_or_else(|poison| poison.into_inner());
    if std::ptr::eq(*active, palette) {
        return false;
    }
    *active = palette;
    true
}

pub fn is_light() -> bool {
    current().light
}

pub fn bg() -> Color32 {
    current().bg
}
pub fn panel() -> Color32 {
    current().panel
}
pub fn panel_dark() -> Color32 {
    current().panel_dark
}
pub fn panel_hover() -> Color32 {
    current().panel_hover
}
pub fn border() -> Color32 {
    current().border
}
pub fn text() -> Color32 {
    current().text
}
pub fn muted_color() -> Color32 {
    current().muted
}
pub fn mint() -> Color32 {
    current().mint
}
pub fn mint_dark() -> Color32 {
    current().mint_dark
}
pub fn danger() -> Color32 {
    current().danger
}
pub fn warn() -> Color32 {
    current().warn
}

/// 安装 palette 对应的 egui 视觉样式（字体已单独设置）。
pub fn apply(ctx: &egui::Context, palette: &'static Palette) {
    set_active(palette);
    let mut style = (*ctx.style()).clone();
    let mut visuals = if palette.light {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };
    visuals.dark_mode = !palette.light;

    visuals.panel_fill = palette.bg;
    visuals.window_fill = palette.panel;
    visuals.extreme_bg_color = palette.panel_dark;
    visuals.faint_bg_color = palette.panel_dark;
    visuals.window_stroke = Stroke::new(1.0, palette.border);
    visuals.window_corner_radius = CornerRadius::same(CARD_RADIUS);
    visuals.menu_corner_radius = CornerRadius::same(10);
    let shadow_alpha = if palette.light { 36 } else { 160 };
    visuals.window_shadow = egui::Shadow {
        offset: [0, 10],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(shadow_alpha),
    };
    visuals.popup_shadow = egui::Shadow {
        offset: [0, 6],
        blur: 22,
        spread: 0,
        color: Color32::from_black_alpha(shadow_alpha / 2),
    };
    visuals.selection.bg_fill = palette.mint;
    visuals.selection.stroke = Stroke::new(1.0, palette.selection_fg);
    visuals.hyperlink_color = palette.mint;
    visuals.text_cursor.stroke = Stroke::new(2.0, palette.mint);
    visuals.slider_trailing_fill = true;

    let radius = CornerRadius::same(SMALL_RADIUS);
    visuals.widgets.noninteractive.bg_fill = palette.panel;
    visuals.widgets.noninteractive.weak_bg_fill = palette.panel;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, palette.border);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, palette.text);
    visuals.widgets.noninteractive.corner_radius = radius;

    visuals.widgets.inactive.bg_fill = palette.panel_dark;
    visuals.widgets.inactive.weak_bg_fill = palette.panel_dark;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, palette.border);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, palette.text);
    visuals.widgets.inactive.corner_radius = radius;
    visuals.widgets.inactive.expansion = 0.0;

    visuals.widgets.hovered.bg_fill = palette.panel_hover;
    visuals.widgets.hovered.weak_bg_fill = palette.panel_hover;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, palette.mint);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, palette.text);
    visuals.widgets.hovered.corner_radius = radius;
    visuals.widgets.hovered.expansion = 0.0;

    visuals.widgets.active.bg_fill = palette.panel_hover;
    visuals.widgets.active.weak_bg_fill = palette.panel_hover;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, palette.mint);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, palette.text);
    visuals.widgets.active.corner_radius = radius;
    visuals.widgets.active.expansion = 0.0;

    visuals.widgets.open.bg_fill = palette.panel_hover;
    visuals.widgets.open.weak_bg_fill = palette.panel_hover;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0, palette.mint);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, palette.text);
    visuals.widgets.open.corner_radius = radius;

    style.visuals = visuals;
    style.spacing.item_spacing = Vec2::new(10.0, 10.0);
    style.spacing.button_padding = Vec2::new(14.0, 8.0);
    style.spacing.interact_size = Vec2::new(40.0, 32.0);
    style.spacing.window_margin = Margin::same(16);
    style.spacing.menu_margin = Margin::same(10);
    style.spacing.indent = 18.0;
    style.spacing.scroll.bar_width = 8.0;
    style.spacing.scroll.bar_inner_margin = 4.0;

    style
        .text_styles
        .insert(egui::TextStyle::Heading, FontId::proportional(22.0));
    style
        .text_styles
        .insert(egui::TextStyle::Body, FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Small, FontId::proportional(12.0));
    style
        .text_styles
        .insert(egui::TextStyle::Monospace, FontId::monospace(12.0));

    ctx.set_style(style);
}

/// 极淡网格背景（在中央内容区底层绘制）；浅色下更淡。
pub fn grid(ui: &Ui) {
    let rect = ui.max_rect();
    let painter = ui.painter();
    let color = current().grid;
    let step = 40.0;
    let mut x = rect.left();
    while x <= rect.right() {
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            Stroke::new(1.0, color),
        );
        x += step;
    }
    let mut y = rect.top();
    while y <= rect.bottom() {
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            Stroke::new(1.0, color),
        );
        y += step;
    }
}

/// 卡片容器：panel 背景 + 1px 边框 + 12 圆角 + 18 内边距。
pub fn card(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    card_titled(ui, "", add);
}

/// 带标题的卡片。
pub fn card_titled(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    let frame = Frame::new()
        .fill(panel())
        .stroke(Stroke::new(1.0, border()))
        .corner_radius(CornerRadius::same(CARD_RADIUS))
        .inner_margin(Margin::same(18));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        if !title.is_empty() {
            ui.label(RichText::new(title).size(15.0).strong().color(text()));
            ui.add_space(10.0);
        }
        add(ui);
    });
    ui.add_space(12.0);
}

/// 页面标题 + 副标题。
pub fn page_header(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.label(RichText::new(title).size(24.0).strong().color(text()));
    if !subtitle.is_empty() {
        ui.label(RichText::new(subtitle).color(muted_color()));
    }
    ui.add_space(14.0);
}

/// 小标题（卡片内）。
pub fn section(ui: &mut Ui, title: &str) {
    ui.label(RichText::new(title).size(14.0).strong().color(text()));
    ui.add_space(6.0);
}

/// 主按钮：薄荷绿填充 + 高对比文字。
pub fn primary_button(ui: &mut Ui, label: &str) -> bool {
    ui.add(
        egui::Button::new(RichText::new(label).color(mint_dark()).strong())
            .fill(mint())
            .stroke(Stroke::NONE)
            .corner_radius(CornerRadius::same(SMALL_RADIUS))
            .min_size(Vec2::new(0.0, 34.0)),
    )
    .clicked()
}

/// 次要按钮：面板底色 + 描边。
pub fn secondary_button(ui: &mut Ui, label: &str) -> bool {
    ui.add(
        egui::Button::new(RichText::new(label).color(text()))
            .fill(panel())
            .stroke(Stroke::new(1.0, border()))
            .corner_radius(CornerRadius::same(SMALL_RADIUS))
            .min_size(Vec2::new(0.0, 32.0)),
    )
    .clicked()
}

/// 危险按钮：危险色文字 + 描边。
pub fn danger_button(ui: &mut Ui, label: &str) -> bool {
    let stroke = if current().light {
        Color32::from_rgb(0xF3, 0xB6, 0xB6)
    } else {
        Color32::from_rgb(0x5C, 0x2A, 0x2A)
    };
    ui.add(
        egui::Button::new(RichText::new(label).color(danger()))
            .fill(panel())
            .stroke(Stroke::new(1.0, stroke))
            .corner_radius(CornerRadius::same(SMALL_RADIUS))
            .min_size(Vec2::new(0.0, 30.0)),
    )
    .clicked()
}

/// 品牌 logo：薄荷绿圆角方块 + 折线（几何绘制，无图片资源）。
pub fn logo(ui: &mut Ui, size: f32) {
    let (rect, _response) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same((size * 0.28) as u8), mint());
    let center = rect.center();
    let points = vec![
        egui::pos2(center.x - size * 0.18, center.y + size * 0.02),
        egui::pos2(center.x - size * 0.02, center.y - size * 0.22),
        egui::pos2(center.x + size * 0.06, center.y - size * 0.02),
        egui::pos2(center.x + size * 0.20, center.y - size * 0.26),
    ];
    painter.add(egui::Shape::line(
        points,
        Stroke::new((size * 0.09).max(2.0), mint_dark()),
    ));
}

/// 版本徽章（胶囊）。
pub fn pill(ui: &mut Ui, label: &str) {
    Frame::new()
        .fill(panel_dark())
        .stroke(Stroke::new(1.0, border()))
        .corner_radius(CornerRadius::same(255))
        .inner_margin(Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.label(
                RichText::new(label)
                    .size(12.0)
                    .color(muted_color())
                    .monospace(),
            );
        });
}

/// 状态点 + 文本。
pub fn status_dot(ui: &mut Ui, color: Color32, label: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, color);
        ui.label(RichText::new(label).color(color));
    });
}

/// 品牌栏内容（logo + 名称 + 副标题 + 版本徽章）。
pub fn brand_row(ui: &mut Ui, subtitle: &str, version: &str) {
    ui.horizontal(|ui| {
        logo(ui, 38.0);
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.label(
                RichText::new("drive-sync")
                    .size(21.0)
                    .strong()
                    .color(text()),
            );
            ui.label(RichText::new(subtitle).size(12.0).color(muted_color()));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            pill(ui, version);
        });
    });
}

/// 键值行。
pub fn kv(ui: &mut Ui, label: &str, value: impl Into<String>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(muted_color()));
        ui.label(RichText::new(value.into()).color(text()));
    });
}

/// 次要说明文字。
pub fn muted(text_text: impl Into<String>) -> RichText {
    RichText::new(text_text.into()).color(muted_color())
}

/// 单行提示（成功/失败/警告）。
pub fn hint(ui: &mut Ui, ok: Option<bool>, label: &str) {
    let color = match ok {
        Some(true) => mint(),
        Some(false) => danger(),
        None => warn(),
    };
    ui.label(RichText::new(label).color(color));
}

/// 空状态占位。
pub fn empty_state(ui: &mut Ui, label: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(label).color(muted_color()));
    ui.add_space(8.0);
}

/// 设置页文本输入框（输入框底色/聚焦描边由全局视觉提供）。
pub fn input(ui: &mut Ui, value: &mut String, hint_text: &str, width: f32) -> bool {
    let response = ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(RichText::new(hint_text).color(muted_color()))
            .desired_width(width)
            .margin(Vec2::new(10.0, 8.0)),
    );
    response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter))
}

/// 供对话框绘制圆角背景。
pub fn dialog_frame() -> Frame {
    Frame::new()
        .fill(panel())
        .stroke(Stroke::new(1.0, border()))
        .corner_radius(CornerRadius::same(CARD_RADIUS))
        .inner_margin(Margin::same(18))
}

/// 勾选标记按钮。
pub fn check_mark(ui: &mut Ui, checked: bool, label: &str) -> bool {
    let (fill, color) = if checked {
        (
            if current().light {
                Color32::from_rgb(0xE6, 0xF7, 0xF1)
            } else {
                Color32::from_rgb(0x10, 0x2A, 0x23)
            },
            mint(),
        )
    } else {
        (panel_dark(), muted_color())
    };
    ui.add(
        egui::Button::new(RichText::new(label).color(color))
            .fill(fill)
            .stroke(Stroke::new(1.0, if checked { mint() } else { border() }))
            .corner_radius(CornerRadius::same(SMALL_RADIUS))
            .min_size(Vec2::new(0.0, 30.0)),
    )
    .clicked()
}

/// 分隔线。
pub fn rule(ui: &mut Ui) {
    ui.add_space(4.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::ZERO, border());
    ui.add_space(4.0);
}

/// 等宽日志文本。
pub fn mono(text_text: &str) -> RichText {
    RichText::new(text_text)
        .monospace()
        .size(12.0)
        .color(muted_color())
}

/// 强调文字。
pub fn accent(label: &str) -> RichText {
    RichText::new(label).color(mint())
}

/// 居中内容宽度。
pub fn centered_width(ui: &Ui, max: f32) -> f32 {
    ui.available_width().min(max)
}

/// 右对齐布局辅助。
pub fn right_aligned<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add)
        .inner
}

/// 文本省略（供表格使用）。
pub fn truncate(text_text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (index, ch) in text_text.chars().enumerate() {
        if index >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

/// 大字（SAS/配对码）。
pub fn big_text(ui: &mut Ui, label: &str, color: Color32) {
    ui.label(RichText::new(label).size(26.0).strong().color(color));
}

/// 居中文本。
pub fn centered(ui: &mut Ui, label: &RichText) {
    ui.vertical_centered(|ui| {
        ui.label(label.clone());
    });
}

pub fn _align_center() -> Align2 {
    Align2::CENTER_CENTER
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_mode_serde_is_lowercase() {
        assert_eq!(
            serde_json::to_string(&ThemeMode::System).expect("序列化"),
            "\"system\""
        );
        assert_eq!(
            serde_json::from_str::<ThemeMode>("\"light\"").expect("反序列化"),
            ThemeMode::Light
        );
        // 未知值回退默认（跟随系统）。
        assert_eq!(
            serde_json::from_str::<GuiSettings>("{}")
                .expect("空对象")
                .theme,
            ThemeMode::System
        );
    }

    #[test]
    fn palette_resolution_for_modes() {
        assert!(!ThemeMode::Dark.palette(Some(true)).light);
        assert!(ThemeMode::Light.palette(None).light);
        assert!(!ThemeMode::System.palette(None).light, "未知系统回退深色");
        assert!(ThemeMode::System.palette(Some(true)).light);
        assert!(!ThemeMode::System.palette(Some(false)).light);
    }

    #[test]
    fn gui_settings_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "drive-gui-theme-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        // 无文件 → 默认跟随系统。
        assert_eq!(load_gui_settings(&dir).theme, ThemeMode::System);
        save_gui_settings(
            &dir,
            &GuiSettings {
                theme: ThemeMode::Light,
            },
        )
        .expect("保存 gui.json");
        assert_eq!(load_gui_settings(&dir).theme, ThemeMode::Light);
        let raw = std::fs::read_to_string(gui_settings_path(&dir)).expect("读取 gui.json");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("解析 JSON");
        assert_eq!(value["theme"], "light");
        // 无效 JSON 不 panic，回退默认。
        std::fs::write(gui_settings_path(&dir), "{oops").expect("写入坏 JSON");
        assert_eq!(load_gui_settings(&dir).theme, ThemeMode::System);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
