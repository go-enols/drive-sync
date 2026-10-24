//! PolyBot 风格主题：深色背景 + 卡片分区 + 薄荷绿主色。
//!
//! 颜色取自设计规范：
//! 背景 `#0B0F14`、面板 `#111820`、边框 `#1E2A33`、主文字 `#E6EDF3`、次要文字 `#9CA3AF`、
//! 主色薄荷绿 `#22E3B0`（按钮文字深色 `#08110C`）、危险色 `#F87171`。
#![allow(dead_code)]

use egui::{
    Align2, Color32, CornerRadius, FontId, Frame, Margin, RichText, Sense, Stroke, Ui, Vec2,
};

pub const BG: Color32 = Color32::from_rgb(0x0B, 0x0F, 0x14);
pub const PANEL: Color32 = Color32::from_rgb(0x11, 0x18, 0x20);
pub const PANEL_DARK: Color32 = Color32::from_rgb(0x0E, 0x14, 0x1A);
pub const PANEL_HOVER: Color32 = Color32::from_rgb(0x16, 0x22, 0x2B);
pub const BORDER: Color32 = Color32::from_rgb(0x1E, 0x2A, 0x33);
pub const TEXT: Color32 = Color32::from_rgb(0xE6, 0xED, 0xF3);
pub const MUTED: Color32 = Color32::from_rgb(0x9C, 0xA3, 0xAF);
pub const MINT: Color32 = Color32::from_rgb(0x22, 0xE3, 0xB0);
pub const MINT_DARK: Color32 = Color32::from_rgb(0x08, 0x11, 0x0C);
pub const DANGER: Color32 = Color32::from_rgb(0xF8, 0x71, 0x71);
pub const WARN: Color32 = Color32::from_rgb(0xFB, 0xBF, 0x24);

pub const CARD_RADIUS: u8 = 12;
pub const SMALL_RADIUS: u8 = 8;

/// 全局安装暗色主题（在字体设置之后调用）。
pub fn apply(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let mut visuals = egui::Visuals::dark();

    visuals.panel_fill = BG;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = PANEL_DARK;
    visuals.faint_bg_color = Color32::from_rgb(0x0F, 0x15, 0x1B);
    visuals.window_stroke = Stroke::new(1.0, BORDER);
    visuals.window_corner_radius = CornerRadius::same(CARD_RADIUS);
    visuals.menu_corner_radius = CornerRadius::same(10);
    visuals.window_shadow = egui::Shadow {
        offset: [0, 12],
        blur: 32,
        spread: 0,
        color: Color32::from_black_alpha(160),
    };
    visuals.popup_shadow = egui::Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(140),
    };
    visuals.selection.bg_fill = MINT;
    visuals.selection.stroke = Stroke::new(1.0, MINT_DARK);
    visuals.hyperlink_color = MINT;
    visuals.text_cursor.stroke = Stroke::new(2.0, MINT);
    visuals.slider_trailing_fill = true;

    let radius = CornerRadius::same(SMALL_RADIUS);
    visuals.widgets.noninteractive.bg_fill = PANEL;
    visuals.widgets.noninteractive.weak_bg_fill = PANEL;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    visuals.widgets.noninteractive.corner_radius = radius;

    visuals.widgets.inactive.bg_fill = PANEL_DARK;
    visuals.widgets.inactive.weak_bg_fill = PANEL_DARK;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    visuals.widgets.inactive.corner_radius = radius;
    visuals.widgets.inactive.expansion = 0.0;

    visuals.widgets.hovered.bg_fill = PANEL_HOVER;
    visuals.widgets.hovered.weak_bg_fill = PANEL_HOVER;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, MINT);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    visuals.widgets.hovered.corner_radius = radius;
    visuals.widgets.hovered.expansion = 0.0;

    visuals.widgets.active.bg_fill = Color32::from_rgb(0x1B, 0x2A, 0x33);
    visuals.widgets.active.weak_bg_fill = Color32::from_rgb(0x1B, 0x2A, 0x33);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, MINT);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, TEXT);
    visuals.widgets.active.corner_radius = radius;
    visuals.widgets.active.expansion = 0.0;

    visuals.widgets.open.bg_fill = PANEL_HOVER;
    visuals.widgets.open.weak_bg_fill = PANEL_HOVER;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0, MINT);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, TEXT);
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

/// 极淡网格背景（在中央内容区底层绘制）。
pub fn grid(ui: &Ui) {
    let rect = ui.max_rect();
    let painter = ui.painter();
    let color = Color32::from_rgb(0x0F, 0x16, 0x1D);
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

/// 卡片容器：深色面板 + 1px 边框 + 12 圆角 + 18 内边距。
pub fn card(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    card_titled(ui, "", add);
}

/// 带标题的卡片。
pub fn card_titled(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    let frame = Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(CARD_RADIUS))
        .inner_margin(Margin::same(18));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        if !title.is_empty() {
            ui.label(RichText::new(title).size(15.0).strong().color(TEXT));
            ui.add_space(10.0);
        }
        add(ui);
    });
    ui.add_space(12.0);
}

/// 页面标题 + 副标题。
pub fn page_header(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.label(RichText::new(title).size(24.0).strong().color(TEXT));
    if !subtitle.is_empty() {
        ui.label(RichText::new(subtitle).color(MUTED));
    }
    ui.add_space(14.0);
}

/// 小标题（卡片内）。
pub fn section(ui: &mut Ui, title: &str) {
    ui.label(RichText::new(title).size(14.0).strong().color(TEXT));
    ui.add_space(6.0);
}

/// 主按钮：薄荷绿填充 + 深色文字。
pub fn primary_button(ui: &mut Ui, text: &str) -> bool {
    ui.add(
        egui::Button::new(RichText::new(text).color(MINT_DARK).strong())
            .fill(MINT)
            .stroke(Stroke::NONE)
            .corner_radius(CornerRadius::same(SMALL_RADIUS))
            .min_size(Vec2::new(0.0, 34.0)),
    )
    .clicked()
}

/// 次要按钮：透明底 + 描边。
pub fn secondary_button(ui: &mut Ui, text: &str) -> bool {
    ui.add(
        egui::Button::new(RichText::new(text).color(TEXT))
            .fill(PANEL_DARK)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(SMALL_RADIUS))
            .min_size(Vec2::new(0.0, 32.0)),
    )
    .clicked()
}

/// 危险按钮：红字 + 深色底。
pub fn danger_button(ui: &mut Ui, text: &str) -> bool {
    ui.add(
        egui::Button::new(RichText::new(text).color(DANGER))
            .fill(PANEL_DARK)
            .stroke(Stroke::new(1.0, Color32::from_rgb(0x5C, 0x2A, 0x2A)))
            .corner_radius(CornerRadius::same(SMALL_RADIUS))
            .min_size(Vec2::new(0.0, 30.0)),
    )
    .clicked()
}

/// 品牌 logo：薄荷绿圆角方块 + 深色折线（几何绘制，无图片资源）。
pub fn logo(ui: &mut Ui, size: f32) {
    let (rect, _response) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same((size * 0.28) as u8), MINT);
    let center = rect.center();
    let points = vec![
        egui::pos2(center.x - size * 0.18, center.y + size * 0.02),
        egui::pos2(center.x - size * 0.02, center.y - size * 0.22),
        egui::pos2(center.x + size * 0.06, center.y - size * 0.02),
        egui::pos2(center.x + size * 0.20, center.y - size * 0.26),
    ];
    painter.add(egui::Shape::line(
        points,
        Stroke::new((size * 0.09).max(2.0), MINT_DARK),
    ));
}

/// 版本徽章（胶囊）。
pub fn pill(ui: &mut Ui, text: &str) {
    Frame::new()
        .fill(PANEL_DARK)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(255))
        .inner_margin(Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(12.0).color(MUTED).monospace());
        });
}

/// 状态点 + 文本。
pub fn status_dot(ui: &mut Ui, color: Color32, text: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, color);
        ui.label(RichText::new(text).color(color));
    });
}

/// 品牌栏内容（logo + 名称 + 副标题 + 版本徽章）。
pub fn brand_row(ui: &mut Ui, subtitle: &str, version: &str) {
    ui.horizontal(|ui| {
        logo(ui, 38.0);
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.label(RichText::new("drive-sync").size(21.0).strong().color(TEXT));
            ui.label(RichText::new(subtitle).size(12.0).color(MUTED));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            pill(ui, version);
        });
    });
}

/// 键值行。
pub fn kv(ui: &mut Ui, label: &str, value: impl Into<String>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(MUTED));
        ui.label(RichText::new(value.into()).color(TEXT));
    });
}

/// 次要说明文字。
pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).color(MUTED)
}

/// 单行提示（成功/失败/警告）。
pub fn hint(ui: &mut Ui, ok: Option<bool>, text: &str) {
    let color = match ok {
        Some(true) => MINT,
        Some(false) => DANGER,
        None => WARN,
    };
    ui.label(RichText::new(text).color(color));
}

/// 空状态占位。
pub fn empty_state(ui: &mut Ui, text: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(text).color(MUTED));
    ui.add_space(8.0);
}

/// 设置页文本输入框（深色底 + 薄荷绿聚焦描边由全局视觉提供）。
pub fn input(ui: &mut Ui, value: &mut String, hint_text: &str, width: f32) -> bool {
    let response = ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(RichText::new(hint_text).color(MUTED))
            .desired_width(width)
            .margin(Vec2::new(10.0, 8.0)),
    );
    response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter))
}

/// 供对话框绘制圆角背景。
pub fn dialog_frame() -> Frame {
    Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(CARD_RADIUS))
        .inner_margin(Margin::same(18))
}

/// 打卡/勾选标记（用几何图形替代字体符号，避免缺字）。
pub fn check_mark(ui: &mut Ui, checked: bool, text: &str) -> bool {
    let button = egui::Button::new(RichText::new(text).color(if checked { MINT } else { MUTED }))
        .fill(if checked {
            Color32::from_rgb(0x10, 0x2A, 0x23)
        } else {
            PANEL_DARK
        })
        .stroke(Stroke::new(1.0, if checked { MINT } else { BORDER }))
        .corner_radius(CornerRadius::same(SMALL_RADIUS))
        .min_size(Vec2::new(0.0, 30.0));
    ui.add(button).clicked()
}

/// 给状态栏绘制一条分隔线。
pub fn rule(ui: &mut Ui) {
    ui.add_space(4.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::ZERO, BORDER);
    ui.add_space(4.0);
}

/// 等宽日志文本。
pub fn mono(text: &str) -> RichText {
    RichText::new(text).monospace().size(12.0).color(MUTED)
}

/// 顶部品牌栏高亮文字。
pub fn accent(text: &str) -> RichText {
    RichText::new(text).color(MINT)
}

/// 居中内容宽度（1000 窗口下卡片最大宽度）。
pub fn centered_width(ui: &Ui, max: f32) -> f32 {
    ui.available_width().min(max)
}

/// 文本对齐辅助：右对齐布局。
pub fn right_aligned<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add)
        .inner
}

/// 文本省略（供表格使用）。
pub fn truncate(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

/// 绘制 ASCII 拼接的 "SAS" 大字。
pub fn big_text(ui: &mut Ui, text: &str, color: Color32) {
    ui.label(RichText::new(text).size(26.0).strong().color(color));
}

/// 绘制一段居中文本。
pub fn centered(ui: &mut Ui, text: &RichText) {
    ui.vertical_centered(|ui| {
        ui.label(text.clone());
    });
}

/// 未使用：保留给后续页面（避免 dead_code）。
pub fn _align_center() -> Align2 {
    Align2::CENTER_CENTER
}
