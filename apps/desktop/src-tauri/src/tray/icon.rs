//! 托盘图标：图标文件在 `icons/tray/`（见那里的 README）；本模块负责按平台、尺寸与状态选图，
//! 并合成没有对应文件的两种临时标记。
//!
//! - **底图**：macOS 模板图（菜单栏自动按明暗着色）；Windows 按任务栏明暗（`CursorScreen::taskbar_light()`，
//!   注册表 `SystemUsesLightTheme`）选 `tray-light*` / `tray-dark*`；Linux 默认 `tray-dark*`（见图标 README）；
//! - **尺寸**（[`IconKey::size`]，见 [`pick_size`]）：Windows 通知区域的图标是 `SM_CXSMICON` 大小
//!   （`CursorScreen::tray_icon_size()`，随主显示器的缩放变化：100 % 16、125 % 20、150 % 24、200 % 32、250 % 40、
//!   300 % 48 px），每一档都有按该尺寸绘制的文件（[`ON_LIGHT`] / [`ON_DARK`] 的尺寸表）。交给系统一张恰好等大的图，
//!   它就不再重采样；介于两档之间（175 % = 28 px）取较大的一档、由系统缩小，比把小图放大清晰。macOS 模板图固定 36 px
//!   （18 pt @2x）；Linux 的宿主自行缩放、取不到实际尺寸，用 32 px；
//! - **状态**（[`Variant`]，图标的文件名后缀）：权限缺失 / 快捷键注册失败 → `-attention`（右下角感叹号角标），
//!   剪贴板记录已暂停 → `-paused`（右下角暂停角标），其余 → 正常图；
//! - **运行期合成的圆点**（[`Dot`]，没有对应的图标文件）：「有新版本」→ 右下角蓝点；疑似密钥被跳过时闪一次 → 琥珀色点
//!   （[`Dot::Flash`]，约 0.9 s）。圆点四周挖出透明环与字形分开；模板图只能单色，圆点画成黑色、由系统着色。圆点的大小
//!   按边长等比，并有下限（见 [`DotGeometry`]），16 px 上仍是一个看得出的点；
//! - **Linux 的描边**：SNI / AppIndicator 取不到面板明暗（GNOME 深色顶栏、KDE Breeze 浅色面板、Xfce / MATE 浅色主题都
//!   有），白色的 `tray-dark*` 在浅色面板上几乎看不见。因此 Linux 上在 `tray-dark*`
//!   （含 `-paused` / `-attention`）外面加一圈半透明深色描边（[`Glyph::Outlined`]）：深色面板上几乎看不出，浅色面板上
//!   勾出轮廓。描边粗细同样按边长等比（见 [`outline_radius`]）。将来有专门的 Linux 字形时，替换 `Glyph::bytes` 的映射即可。
//!
//! 合成在内存里完成（48 px 以内的 RGBA，微秒级），只在尺寸或状态变化时进行（[`IconKey`] 相同就不重新设置图标）。

use tauri::image::Image;
use wisp_types::OsKind;

/// 一组状态图：`[正常, -paused, -attention]`
type States = [&'static [u8]; 3];

/// 一组状态图的三个文件：`$stem` 是文件名前缀（`tray-light` 等），`$suffix` 是尺寸后缀（32 px 与模板图没有后缀）
macro_rules! states {
    ($stem:literal, $suffix:literal) => {
        [
            include_bytes!(concat!("../../icons/tray/", $stem, $suffix, ".png")),
            include_bytes!(concat!("../../icons/tray/", $stem, "-paused", $suffix, ".png")),
            include_bytes!(concat!("../../icons/tray/", $stem, "-attention", $suffix, ".png")),
        ]
    };
}

/// Windows / Linux 字形的尺寸表：`(边长 px, 状态图)`，按边长升序。选图（[`pick_size`]）只查这张表，
/// 增减一档只改这里
type SizedStates = [(u32, States); 6];

macro_rules! sized {
    ($stem:literal) => {
        [
            (16, states!($stem, "-16")),
            (20, states!($stem, "-20")),
            (24, states!($stem, "-24")),
            (32, states!($stem, "")),
            (40, states!($stem, "-40")),
            (48, states!($stem, "-48")),
        ]
    };
}

/// macOS 模板图（只有 [`TEMPLATE_SIZE`] 一种尺寸）
const TEMPLATE: States = states!("tray-template", "");
/// 浅色任务栏用的深色字形
const ON_LIGHT: SizedStates = sized!("tray-light");
/// 深色任务栏用的白色字形（Linux 的描边版也用它）
const ON_DARK: SizedStates = sized!("tray-dark");

/// macOS 模板图的边长（18 pt @2x）
pub const TEMPLATE_SIZE: u32 = 36;
/// 取不到托盘实际尺寸时（Linux、查询失败）用的边长：无后缀的文件，也是圆点与描边参数的基准尺寸
pub const DEFAULT_SIZE: u32 = 32;

/// 按托盘要的边长（`CursorScreen::tray_icon_size()`）选文件的边长：有恰好等大的一档就用它；介于两档之间取**较大**的
/// 一档（交给系统缩小，比把小一档放大清晰）；比最大一档还大时用最大一档（400 % 这类极少见的缩放由系统放大）；
/// 没有尺寸（macOS / Linux / 查询失败）时用 [`DEFAULT_SIZE`]
pub fn pick_size(wanted: Option<u32>) -> u32 {
    let Some(wanted) = wanted.filter(|w| *w > 0) else {
        return DEFAULT_SIZE;
    };
    ON_DARK
        .iter()
        .map(|(size, _)| *size)
        .find(|size| *size >= wanted)
        .or_else(|| ON_DARK.last().map(|(size, _)| *size))
        .unwrap_or(DEFAULT_SIZE)
}

/// 尺寸表里这一档的状态图（`size` 先经 [`pick_size`] 落到表里的一档，表外的值也有图可用）
fn sized(table: &'static SizedStates, size: u32) -> &'static States {
    let size = pick_size(Some(size));
    table
        .iter()
        .find(|(s, _)| *s == size)
        .map_or(&table[0].1, |(_, states)| states)
}

/// 图标的状态（对应图标的文件名后缀）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Variant {
    /// 正常
    Normal,
    /// `-paused`：剪贴板记录已暂停
    Paused,
    /// `-attention`：权限缺失 / 快捷键注册失败
    Attention,
}

impl Variant {
    const fn index(self) -> usize {
        match self {
            Self::Normal => 0,
            Self::Paused => 1,
            Self::Attention => 2,
        }
    }
}

/// 运行期合成的圆点（没有对应的状态图）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dot {
    /// 有新版本（蓝）
    Update,
    /// 闪烁帧：疑似密钥被跳过（琥珀色）
    Flash,
}

impl Dot {
    /// 圆点颜色（RGB）
    pub const fn color(self) -> [u8; 3] {
        match self {
            Self::Update => [0x3B, 0x82, 0xF6],
            Self::Flash => [0xFA, 0xCC, 0x15],
        }
    }
}

/// 一个托盘图标的全部决定因素（相同即不必重新设置图标）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IconKey {
    pub glyph: Glyph,
    /// 边长（px，[`Glyph::size_for`] 选出的一档）：缩放变化使通知区域的图标尺寸变了，也要重新设置图标
    pub size: u32,
    pub variant: Variant,
    pub dot: Option<Dot>,
}

/// 底图的选择
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Glyph {
    /// macOS 模板图
    Template,
    /// 浅色任务栏用的深色字形
    OnLight,
    /// 深色任务栏用的白色字形
    OnDark,
    /// Linux：白色字形 + 深色描边，浅色与深色面板上都清晰
    Outlined,
}

impl Glyph {
    /// 当前平台的底图：`taskbar_light` 只在 Windows 上有值；Linux 取不到面板明暗，一律用带描边的白色字形
    pub fn for_platform(os: OsKind, taskbar_light: Option<bool>) -> Self {
        match os {
            OsKind::Macos => Self::Template,
            OsKind::Windows => {
                if taskbar_light.unwrap_or(false) {
                    Self::OnLight
                } else {
                    Self::OnDark
                }
            }
            OsKind::Linux => Self::Outlined,
        }
    }

    /// 这个底图实际使用的边长：模板图只有 [`TEMPLATE_SIZE`] 一种；其余按托盘要的边长 [`pick_size`]
    pub fn size_for(self, wanted: Option<u32>) -> u32 {
        if self.is_template() {
            TEMPLATE_SIZE
        } else {
            pick_size(wanted)
        }
    }

    fn bytes(self, size: u32, variant: Variant) -> &'static [u8] {
        let states = match self {
            Self::Template => &TEMPLATE,
            Self::OnLight => sized(&ON_LIGHT, size),
            Self::OnDark | Self::Outlined => sized(&ON_DARK, size),
        };
        states[variant.index()]
    }

    /// 是否按模板图显示
    pub const fn is_template(self) -> bool {
        matches!(self, Self::Template)
    }
}

/// 描边颜色（接近黑的中性灰，不透明度见 [`OUTLINE_ALPHA`]）
const OUTLINE_RGB: [u8; 3] = [0x1c, 0x1c, 0x1f];
/// 描边的最大不透明度：浅色面板上足够勾出轮廓，深色面板上几乎看不出
const OUTLINE_ALPHA: f64 = 0.85;
/// 描边半径（px，[`DEFAULT_SIZE`] 的图标；字形边缘是抗锯齿的半透明像素，半径要盖过它一整圈）
const OUTLINE_RADIUS: f64 = 1.75;
/// 描边半径的下限：紧挨着字形的一圈像素（距离 1 px）必须被完全盖住，否则 16–24 px 上轮廓断断续续
const MIN_OUTLINE_RADIUS: f64 = 1.5;

/// 边长为 `size` 的图标的描边半径：按边长等比（各尺寸看起来一样粗），不小于 [`MIN_OUTLINE_RADIUS`]
fn outline_radius(size: u32) -> f64 {
    (OUTLINE_RADIUS * f64::from(size) / f64::from(DEFAULT_SIZE)).max(MIN_OUTLINE_RADIUS)
}

/// 生成托盘图标：底图（按尺寸与状态）+ Linux 描边 + 可选的合成圆点
pub fn render(key: IconKey) -> tauri::Result<Image<'static>> {
    let IconKey {
        glyph,
        size,
        variant,
        dot,
    } = key;
    let base = Image::from_bytes(glyph.bytes(size, variant))?;
    if dot.is_none() && glyph != Glyph::Outlined {
        return Ok(base.to_owned());
    }
    let (w, h) = (base.width(), base.height());
    let mut rgba = if glyph == Glyph::Outlined {
        outline(base.rgba(), w, h)
    } else {
        base.rgba().to_vec()
    };
    let Some(dot) = dot else {
        return Ok(Image::new_owned(rgba, w, h));
    };
    // 模板图只能单色：圆点用纯黑（系统按菜单栏明暗着色）
    let color = if glyph.is_template() { [0, 0, 0] } else { dot.color() };
    draw_dot(&mut rgba, w, h, color);
    Ok(Image::new_owned(rgba, w, h))
}

/// 右下角圆点的几何（px）
#[derive(Debug, Clone, Copy, PartialEq)]
struct DotGeometry {
    /// 圆心
    cx: f64,
    cy: f64,
    /// 半径：边长的 22 %（32 px 上约 7 px），不小于 3 px——16 px 上仍是一个看得出的点
    r: f64,
    /// 圆点外透明环的宽度：边长的 4.5 %，不小于 1.5 px——小尺寸上也和字形分得开
    gap: f64,
}

impl DotGeometry {
    fn new(width: u32, height: u32) -> Self {
        let (w, h) = (f64::from(width), f64::from(height));
        let size = w.min(h);
        let r = (size * 0.22).max(3.0);
        let gap = (size * 0.045).max(1.5);
        // 圆心离右、下边缘 r + 0.5：抗锯齿的外缘正好落在图像边界上，圆点贴角而不被裁掉
        Self {
            cx: w - r - 0.5,
            cy: h - r - 0.5,
            r,
            gap,
        }
    }
}

/// 在右下角画抗锯齿圆点，并在圆点四周挖出透明环
pub fn draw_dot(rgba: &mut [u8], width: u32, height: u32, color: [u8; 3]) {
    let DotGeometry { cx, cy, r, gap } = DotGeometry::new(width, height);
    for y in 0..height {
        for x in 0..width {
            let (px, py) = (f64::from(x) + 0.5, f64::from(y) + 0.5);
            let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
            let Ok(i) = usize::try_from((y * width + x) * 4) else {
                continue;
            };
            let Some(px) = rgba.get_mut(i..i + 4) else { continue };
            // 透明环：按距离渐隐原像素
            let clear = (r + gap + 0.5 - d).clamp(0.0, 1.0);
            if clear > 0.0 {
                px[3] = scale(px[3], 1.0 - clear);
            }
            // 圆点：抗锯齿覆盖率
            let cover = (r + 0.5 - d).clamp(0.0, 1.0);
            if cover > 0.0 {
                blend(px, color, cover);
            }
        }
    }
}

/// 在字形下面垫一圈描边：描边的不透明度 = 半径内字形 alpha 的最大值（按距离抗锯齿）× [`OUTLINE_ALPHA`]，再把原字形
/// 叠在上面（非预乘 RGBA）。半径按边长取 [`outline_radius`]
fn outline(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let radius = outline_radius(width.min(height));
    let alpha = |x: usize, y: usize| rgba.get((y * w + x) * 4 + 3).copied().unwrap_or(0);
    let reach = radius.ceil() as isize + 1;
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let mut halo = 0.0f64;
            for dy in -reach..=reach {
                for dx in -reach..=reach {
                    let (sx, sy) = (x as isize + dx, y as isize + dy);
                    if sx < 0 || sy < 0 || sx >= w as isize || sy >= h as isize {
                        continue;
                    }
                    let d = ((dx * dx + dy * dy) as f64).sqrt();
                    let cover = (radius + 0.5 - d).clamp(0.0, 1.0);
                    if cover > 0.0 {
                        halo = halo.max(f64::from(alpha(sx as usize, sy as usize)) / 255.0 * cover);
                    }
                }
            }
            let i = (y * w + x) * 4;
            let px = &mut out[i..i + 4];
            px[..3].copy_from_slice(&OUTLINE_RGB);
            px[3] = (halo * OUTLINE_ALPHA * 255.0).round().clamp(0.0, 255.0) as u8;
            let src = &rgba[i..i + 4];
            if src[3] > 0 {
                blend(px, [src[0], src[1], src[2]], f64::from(src[3]) / 255.0);
            }
        }
    }
    out
}

fn scale(v: u8, f: f64) -> u8 {
    // f ∈ [0, 1]，结果 ∈ [0, 255]
    (f64::from(v) * f).round().clamp(0.0, 255.0) as u8
}

/// 以覆盖率 `a` 把颜色叠到像素上（非预乘 RGBA 的 over 运算）
fn blend(px: &mut [u8], color: [u8; 3], a: f64) {
    let dst_a = f64::from(px[3]) / 255.0;
    let out_a = a + dst_a * (1.0 - a);
    if out_a <= 0.0 {
        return;
    }
    for c in 0..3 {
        let src = f64::from(color[c]);
        let dst = f64::from(px[c]);
        px[c] = ((src * a + dst * dst_a * (1.0 - a)) / out_a).round().clamp(0.0, 255.0) as u8;
    }
    px[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_at(img: &Image<'_>, x: u32, y: u32) -> u8 {
        let i = usize::try_from((y * img.width() + x) * 4 + 3).expect("index");
        img.rgba()[i]
    }

    fn key(glyph: Glyph, variant: Variant, dot: Option<Dot>) -> IconKey {
        sized_key(glyph, DEFAULT_SIZE, variant, dot)
    }

    fn sized_key(glyph: Glyph, size: u32, variant: Variant, dot: Option<Dot>) -> IconKey {
        IconKey {
            glyph,
            size: glyph.size_for(Some(size)),
            variant,
            dot,
        }
    }

    const GLYPHS: [Glyph; 4] = [Glyph::Template, Glyph::OnLight, Glyph::OnDark, Glyph::Outlined];
    const VARIANTS: [Variant; 3] = [Variant::Normal, Variant::Paused, Variant::Attention];
    /// 尺寸表里的各档（16 … 48）
    fn sizes() -> Vec<u32> {
        ON_DARK.iter().map(|(size, _)| *size).collect()
    }

    #[test]
    fn size_selection_takes_the_exact_or_next_larger_step() {
        // 恰好有这一档：100 % 16、125 % 20、150 % 24、200 % 32、250 % 40、300 % 48
        for exact in [16, 20, 24, 32, 40, 48] {
            assert_eq!(pick_size(Some(exact)), exact);
        }
        // 介于两档之间取较大的一档：175 % = 28 px → 32，225 % = 36 px → 40，275 % = 44 px → 48；以及非标准值
        for (wanted, picked) in [
            (28, 32),
            (36, 40),
            (44, 48),
            (17, 20),
            (21, 24),
            (25, 32),
            (33, 40),
            (41, 48),
        ] {
            assert_eq!(pick_size(Some(wanted)), picked, "{wanted}");
        }
        // 超出范围：比最小档还小用 16，比最大档还大用 48；没有尺寸或 0 用无后缀的 32
        for (wanted, picked) in [(1, 16), (12, 16), (49, 48), (56, 48), (64, 48), (u32::MAX, 48)] {
            assert_eq!(pick_size(Some(wanted)), picked, "{wanted}");
        }
        assert_eq!(pick_size(None), DEFAULT_SIZE);
        assert_eq!(pick_size(Some(0)), DEFAULT_SIZE);
        // 模板图只有 36 px；Windows / Linux 字形按尺寸表
        assert_eq!(Glyph::Template.size_for(Some(24)), TEMPLATE_SIZE);
        assert_eq!(Glyph::Template.size_for(None), TEMPLATE_SIZE);
        assert_eq!(Glyph::OnDark.size_for(Some(24)), 24);
        assert_eq!(Glyph::OnLight.size_for(Some(28)), 32);
        assert_eq!(Glyph::Outlined.size_for(None), 32);
    }

    #[test]
    fn size_tables_match_and_ascend() {
        let light: Vec<u32> = ON_LIGHT.iter().map(|(size, _)| *size).collect();
        assert_eq!(light, sizes(), "浅色与深色字形的尺寸表必须一致");
        assert!(sizes().windows(2).all(|w| w[0] < w[1]), "尺寸表按边长升序");
        assert!(sizes().contains(&DEFAULT_SIZE));
    }

    #[test]
    fn every_size_and_state_decodes_to_its_size() {
        for glyph in [Glyph::OnLight, Glyph::OnDark, Glyph::Outlined] {
            for size in sizes() {
                let images: Vec<Image<'_>> = VARIANTS
                    .iter()
                    .map(|v| render(sized_key(glyph, size, *v, None)).expect("decode"))
                    .collect();
                for (img, v) in images.iter().zip(VARIANTS) {
                    assert_eq!((img.width(), img.height()), (size, size), "{glyph:?} {v:?} {size}");
                    assert_eq!(img.rgba().len(), (size * size * 4) as usize);
                }
                // 三种状态是三张不同的图（-paused / -attention 带角标）
                assert_ne!(images[0].rgba(), images[1].rgba(), "{glyph:?} {size} paused");
                assert_ne!(images[0].rgba(), images[2].rgba(), "{glyph:?} {size} attention");
                assert_ne!(images[1].rgba(), images[2].rgba(), "{glyph:?} {size}");
            }
        }
        // 浅色任务栏用深色字形、深色任务栏用白色字形：两张表没有接反
        let mean_luma = |img: &Image<'_>| {
            let opaque: Vec<f64> = img
                .rgba()
                .chunks_exact(4)
                .filter(|p| p[3] == 255)
                .map(|p| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]))
                .collect();
            opaque.iter().sum::<f64>() / opaque.len().max(1) as f64
        };
        for size in sizes() {
            for v in VARIANTS {
                let light = render(sized_key(Glyph::OnLight, size, v, None)).expect("light");
                let dark = render(sized_key(Glyph::OnDark, size, v, None)).expect("dark");
                assert!(mean_luma(&light) < 96.0, "{size} {v:?}: {}", mean_luma(&light));
                assert!(mean_luma(&dark) > 192.0, "{size} {v:?}: {}", mean_luma(&dark));
            }
        }
    }

    #[test]
    fn every_asset_decodes_and_states_differ() {
        for glyph in GLYPHS {
            let images: Vec<Image<'_>> = VARIANTS
                .iter()
                .map(|v| render(key(glyph, *v, None)).expect("decode"))
                .collect();
            for img in &images {
                assert!(img.width() >= 16 && img.height() >= 16, "{glyph:?}");
                assert_eq!(img.rgba().len(), (img.width() * img.height() * 4) as usize);
            }
            assert_ne!(images[0].rgba(), images[1].rgba(), "{glyph:?} paused");
            assert_ne!(images[0].rgba(), images[2].rgba(), "{glyph:?} attention");
            assert_ne!(images[1].rgba(), images[2].rgba(), "{glyph:?}");
        }
        // 模板图 36 px（18 pt @2x），不管托盘要多大；Windows / Linux 默认 32 px
        for wanted in [16, 24, 48] {
            let t = render(sized_key(Glyph::Template, wanted, Variant::Normal, None)).expect("t");
            assert_eq!((t.width(), t.height()), (TEMPLATE_SIZE, TEMPLATE_SIZE));
        }
        assert_eq!(
            render(key(Glyph::OnDark, Variant::Normal, None)).expect("d").width(),
            32
        );
    }

    #[test]
    fn glyph_selection_per_platform() {
        assert_eq!(Glyph::for_platform(OsKind::Macos, Some(true)), Glyph::Template);
        assert_eq!(Glyph::for_platform(OsKind::Windows, Some(true)), Glyph::OnLight);
        assert_eq!(Glyph::for_platform(OsKind::Windows, Some(false)), Glyph::OnDark);
        assert_eq!(Glyph::for_platform(OsKind::Linux, None), Glyph::Outlined);
        assert_eq!(Glyph::for_platform(OsKind::Linux, Some(true)), Glyph::Outlined);
        assert!(Glyph::Template.is_template() && !Glyph::OnDark.is_template());
        // Linux 用深色任务栏图（tray-dark*），各尺寸都一样
        for size in sizes() {
            for v in VARIANTS {
                assert_eq!(Glyph::Outlined.bytes(size, v), Glyph::OnDark.bytes(size, v));
            }
        }
        // 表外的边长落到较大的一档
        assert_eq!(
            Glyph::OnDark.bytes(28, Variant::Normal),
            Glyph::OnDark.bytes(32, Variant::Normal)
        );
    }

    #[test]
    fn dot_is_drawn_in_the_bottom_right_corner() {
        for size in sizes() {
            let plain = render(sized_key(Glyph::OnDark, size, Variant::Normal, None)).expect("plain");
            let dotted = render(sized_key(Glyph::OnDark, size, Variant::Normal, Some(Dot::Update))).expect("dot");
            let (w, h) = (dotted.width(), dotted.height());
            // 圆点中心不透明且是蓝色
            let g = DotGeometry::new(w, h);
            let (cx, cy) = (g.cx as u32, g.cy as u32);
            let i = usize::try_from((cy * w + cx) * 4).expect("index");
            assert_eq!(&dotted.rgba()[i..i + 4], &[0x3B, 0x82, 0xF6, 0xFF], "{size}");
            // 左上角不受影响
            assert_eq!(alpha_at(&dotted, 1, 1), alpha_at(&plain, 1, 1), "{size}");
            assert_ne!(plain.rgba(), dotted.rgba(), "{size}");
        }
    }

    #[test]
    fn dots_stay_inside_the_icon_at_every_size() {
        for size in sizes() {
            let g = DotGeometry::new(size, size);
            let s = f64::from(size);
            // 圆点（含抗锯齿外缘）与透明环都在图内
            assert!(
                g.cx + g.r + 0.5 <= s + 1e-9 && g.cy + g.r + 0.5 <= s + 1e-9,
                "{size}: {g:?}"
            );
            assert!(
                g.cx - g.r - g.gap - 0.5 >= 0.0 && g.cy - g.r - g.gap - 0.5 >= 0.0,
                "{size}: {g:?}"
            );
            // 圆点不超过图标的一半、也不小到看不出
            assert!(g.r >= 3.0 && g.r * 2.0 <= s * 0.5, "{size}: {g:?}");
            assert!(g.gap >= 1.5, "{size}: {g:?}");

            // 画在透明底上：覆盖率之和接近完整圆盘的面积（被边缘裁掉时会明显偏小）
            let mut rgba = vec![0u8; (size * size * 4) as usize];
            draw_dot(&mut rgba, size, size, [0xFF, 0, 0]);
            let covered: f64 = rgba.chunks_exact(4).map(|p| f64::from(p[3]) / 255.0).sum();
            let disc = std::f64::consts::PI * g.r * g.r;
            assert!((covered - disc).abs() / disc < 0.05, "{size}: {covered} vs {disc}");

            // 画在铺满的不透明底上（字形最坏的情况）：从圆心往左、往上，先是圆点，再是几乎透明的环，最后回到底色
            let mut rgba = vec![0xFFu8; (size * size * 4) as usize];
            draw_dot(&mut rgba, size, size, [0, 0, 0xFF]);
            let alpha = |x: u32, y: u32| rgba[((y * size + x) * 4 + 3) as usize];
            let (cx, cy) = (g.cx as u32, g.cy as u32);
            for (name, line) in [
                ("row", (0..=cx).rev().map(|x| alpha(x, cy)).collect::<Vec<u8>>()),
                ("column", (0..=cy).rev().map(|y| alpha(cx, y)).collect::<Vec<u8>>()),
            ] {
                assert_eq!(line[0], 255, "{size} {name}: 圆心不透明");
                let ring = line.iter().position(|a| *a < 64);
                assert!(ring.is_some(), "{size} {name}: 圆点与字形之间没有透明环 {line:?}");
                assert_eq!(line.last(), Some(&255), "{size} {name}: 环外是原来的底");
            }
            // 环外（左上角）原样保留
            assert_eq!(alpha(0, 0), 255, "{size}");
        }
    }

    /// 把图标合成到纯色背景上，返回（字形中心像素与背景的最大亮度差）
    fn contrast_on(img: &Image<'_>, bg: [u8; 3]) -> f64 {
        let luma = |c: [f64; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        let bg_luma = luma(bg.map(f64::from));
        img.rgba()
            .chunks_exact(4)
            .map(|p| {
                let a = f64::from(p[3]) / 255.0;
                let c = [0, 1, 2].map(|k| f64::from(p[k]) * a + f64::from(bg[k]) * (1.0 - a));
                (luma(c) - bg_luma).abs()
            })
            .fold(0.0, f64::max)
    }

    #[test]
    fn outlined_glyph_is_legible_on_light_and_dark_panels() {
        let (light, dark) = ([0xf5, 0xf5, 0xf7], [0x24, 0x24, 0x27]);
        for size in sizes() {
            let plain = render(sized_key(Glyph::OnDark, size, Variant::Normal, None)).expect("plain");
            let outlined = render(sized_key(Glyph::Outlined, size, Variant::Normal, None)).expect("outlined");
            // 白色字形在浅色面板上几乎看不见；描边之后有明显对比
            assert!(
                contrast_on(&plain, light) < 40.0,
                "{size}: {}",
                contrast_on(&plain, light)
            );
            assert!(
                contrast_on(&outlined, light) > 140.0,
                "{size}: {}",
                contrast_on(&outlined, light)
            );
            // 深色面板上仍然清晰
            assert!(contrast_on(&outlined, dark) > 150.0, "{size}");
            // 字形本身的像素保持不变、尺寸不变
            assert_eq!((outlined.width(), outlined.height()), (size, size));
            for (o, p) in outlined.rgba().chunks_exact(4).zip(plain.rgba().chunks_exact(4)) {
                if p[3] == 255 {
                    assert_eq!(o, p, "{size}");
                }
                assert!(o[3] >= p[3], "{size}: 描边只增加不透明度");
            }
            let w = size as usize;
            let a = |img: &Image<'_>, x: usize, y: usize| img.rgba()[(y * w + x) * 4 + 3];
            // 轮廓是闭合的：紧挨着不透明字形（上下左右）的透明像素都被描边完全盖住
            for y in 0..w {
                for x in 0..w {
                    if a(&plain, x, y) != 0 {
                        continue;
                    }
                    let touches = [(0, 1), (2, 1), (1, 0), (1, 2)].iter().any(|(dx, dy)| {
                        let (nx, ny) = ((x + dx).wrapping_sub(1), (y + dy).wrapping_sub(1));
                        nx < w && ny < w && a(&plain, nx, ny) == 255
                    });
                    if touches {
                        assert!(a(&outlined, x, y) >= 200, "{size}: ({x}, {y}) {}", a(&outlined, x, y));
                    }
                }
            }
            // 描边只贴着字形，不会糊成一片：新增的像素离最近的字形像素不超过「半径 + 0.5」，半径按边长等比
            let radius = outline_radius(size);
            assert!(
                radius >= MIN_OUTLINE_RADIUS && radius <= f64::from(size) * 0.1,
                "{size}: {radius}"
            );
            let glyph: Vec<(f64, f64)> = (0..w * w)
                .filter(|i| a(&plain, i % w, i / w) > 0)
                .map(|i| ((i % w) as f64, (i / w) as f64))
                .collect();
            for i in (0..w * w).filter(|i| a(&plain, i % w, i / w) == 0 && a(&outlined, i % w, i / w) > 0) {
                let (x, y) = ((i % w) as f64, (i / w) as f64);
                let nearest = glyph
                    .iter()
                    .map(|(gx, gy)| ((gx - x).powi(2) + (gy - y).powi(2)).sqrt())
                    .fold(f64::INFINITY, f64::min);
                assert!(nearest <= radius + 0.5, "{size}: ({x}, {y}) 离字形 {nearest} px");
            }
            // 状态图与圆点照常叠在描边版上
            for v in [Variant::Paused, Variant::Attention] {
                let state = render(sized_key(Glyph::Outlined, size, v, None)).expect("state");
                assert_ne!(state.rgba(), outlined.rgba(), "{size} {v:?}");
            }
            let flashing = render(sized_key(Glyph::Outlined, size, Variant::Normal, Some(Dot::Flash))).expect("flash");
            assert_ne!(flashing.rgba(), outlined.rgba(), "{size}");
        }
    }

    #[test]
    fn template_images_stay_monochrome() {
        for v in VARIANTS {
            let img = render(key(Glyph::Template, v, Some(Dot::Update))).expect("dot");
            for px in img.rgba().chunks_exact(4).filter(|p| p[3] == 255) {
                assert_eq!(&px[..3], &[0, 0, 0], "模板图里不能出现彩色像素（{v:?}）");
            }
        }
    }

    #[test]
    fn blend_over_transparent_pixel_takes_the_color() {
        let mut px = [10, 20, 30, 0];
        blend(&mut px, [200, 100, 50], 1.0);
        assert_eq!(px, [200, 100, 50, 255]);
        let mut px = [0, 0, 0, 0];
        blend(&mut px, [255, 255, 255], 0.5);
        assert_eq!(px, [255, 255, 255, 128]);
    }
}
