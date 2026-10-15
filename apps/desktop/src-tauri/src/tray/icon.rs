//! 托盘图标：图标文件在 `icons/tray/`（见那里的 README）；本模块负责按平台与状态选图，
//! 并合成没有对应文件的两种临时标记。
//!
//! - **底图**：macOS 模板图（菜单栏自动按明暗着色）；Windows 按任务栏明暗（`CursorScreen::taskbar_light()`，
//!   注册表 `SystemUsesLightTheme`）选 `tray-light*` / `tray-dark*`；Linux 默认 `tray-dark*`（见图标 README）；
//! - **状态**（[`Variant`]，图标的文件名后缀）：权限缺失 / 快捷键注册失败 → `-attention`（右下角感叹号角标），
//!   剪贴板记录已暂停 → `-paused`（右下角暂停角标），其余 → 正常图；
//! - **运行期合成的圆点**（[`Dot`]，没有对应的图标文件）：「有新版本」→ 右下角蓝点；疑似密钥被跳过时闪一次 → 琥珀色点
//!   （[`Dot::Flash`]，约 0.9 s）。圆点四周挖出透明环与字形分开；模板图只能单色，圆点画成黑色、由系统着色；
//! - **Linux 的描边**：SNI / AppIndicator 取不到面板明暗（GNOME 深色顶栏、KDE Breeze 浅色面板、Xfce / MATE 浅色主题都
//!   有），白色的 `tray-dark*` 在浅色面板上几乎看不见。因此 Linux 上在 `tray-dark*`
//!   （含 `-paused` / `-attention`）外面加一圈半透明深色描边（[`Glyph::Outlined`]）：深色面板上几乎看不出，浅色面板上
//!   勾出轮廓。将来有专门的 Linux 字形时，替换 `Glyph::bytes` 的映射即可。
//!
//! 合成在内存里完成（36 px 以内的 RGBA，微秒级），只在状态变化时进行。

use tauri::image::Image;
use wisp_types::OsKind;

/// 托盘图标文件：`[正常, -paused, -attention]`
const TEMPLATE: [&[u8]; 3] = [
    include_bytes!("../../icons/tray/tray-template.png"),
    include_bytes!("../../icons/tray/tray-template-paused.png"),
    include_bytes!("../../icons/tray/tray-template-attention.png"),
];
const ON_LIGHT: [&[u8]; 3] = [
    include_bytes!("../../icons/tray/tray-light.png"),
    include_bytes!("../../icons/tray/tray-light-paused.png"),
    include_bytes!("../../icons/tray/tray-light-attention.png"),
];
const ON_DARK: [&[u8]; 3] = [
    include_bytes!("../../icons/tray/tray-dark.png"),
    include_bytes!("../../icons/tray/tray-dark-paused.png"),
    include_bytes!("../../icons/tray/tray-dark-attention.png"),
];

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

    fn bytes(self, variant: Variant) -> &'static [u8] {
        let set = match self {
            Self::Template => &TEMPLATE,
            Self::OnLight => &ON_LIGHT,
            Self::OnDark | Self::Outlined => &ON_DARK,
        };
        set[variant.index()]
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
/// 描边半径（px，32 px 图标；字形边缘是抗锯齿的半透明像素，半径要盖过它一整圈）
const OUTLINE_RADIUS: f64 = 1.75;

/// 生成托盘图标：底图（按状态）+ Linux 描边 + 可选的合成圆点
pub fn render(key: IconKey) -> tauri::Result<Image<'static>> {
    let IconKey { glyph, variant, dot } = key;
    let base = Image::from_bytes(glyph.bytes(variant))?;
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

/// 在右下角画抗锯齿圆点，并在圆点四周挖出透明环
pub fn draw_dot(rgba: &mut [u8], width: u32, height: u32, color: [u8; 3]) {
    let (w, h) = (f64::from(width), f64::from(height));
    let size = w.min(h);
    let r = (size * 0.22).max(3.0);
    let gap = (size * 0.045).max(1.5);
    let (cx, cy) = (w - r - 0.5, h - r - 0.5);
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
/// 叠在上面（非预乘 RGBA）
fn outline(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let alpha = |x: usize, y: usize| rgba.get((y * w + x) * 4 + 3).copied().unwrap_or(0);
    let reach = OUTLINE_RADIUS.ceil() as isize + 1;
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
                    let cover = (OUTLINE_RADIUS + 0.5 - d).clamp(0.0, 1.0);
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
        IconKey { glyph, variant, dot }
    }

    const GLYPHS: [Glyph; 4] = [Glyph::Template, Glyph::OnLight, Glyph::OnDark, Glyph::Outlined];
    const VARIANTS: [Variant; 3] = [Variant::Normal, Variant::Paused, Variant::Attention];

    #[test]
    fn every_wp13_asset_decodes_and_states_differ() {
        for glyph in GLYPHS {
            let images: Vec<Image<'_>> = VARIANTS
                .iter()
                .map(|v| render(key(glyph, *v, None)).expect("decode"))
                .collect();
            for img in &images {
                assert!(img.width() >= 16 && img.height() >= 16, "{glyph:?}");
                assert_eq!(img.rgba().len(), (img.width() * img.height() * 4) as usize);
            }
            // 三种状态是三张不同的图（-paused / -attention 带角标）
            assert_ne!(images[0].rgba(), images[1].rgba(), "{glyph:?} paused");
            assert_ne!(images[0].rgba(), images[2].rgba(), "{glyph:?} attention");
            assert_ne!(images[1].rgba(), images[2].rgba(), "{glyph:?}");
        }
        // 模板图 36 px（18 pt @2x），Windows / Linux 32 px
        assert_eq!(
            render(key(Glyph::Template, Variant::Normal, None)).expect("t").width(),
            36
        );
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
        // Linux 用深色任务栏图（tray-dark*）
        for v in VARIANTS {
            assert_eq!(Glyph::Outlined.bytes(v), Glyph::OnDark.bytes(v));
        }
    }

    #[test]
    fn dot_is_drawn_in_the_bottom_right_corner() {
        let plain = render(key(Glyph::OnDark, Variant::Normal, None)).expect("plain");
        let dotted = render(key(Glyph::OnDark, Variant::Normal, Some(Dot::Update))).expect("dot");
        let (w, h) = (dotted.width(), dotted.height());
        // 圆点中心不透明且是蓝色
        let r = (f64::from(w.min(h)) * 0.22).max(3.0);
        let (cx, cy) = ((f64::from(w) - r - 0.5) as u32, (f64::from(h) - r - 0.5) as u32);
        let i = usize::try_from((cy * w + cx) * 4).expect("index");
        assert_eq!(&dotted.rgba()[i..i + 4], &[0x3B, 0x82, 0xF6, 0xFF]);
        // 左上角不受影响
        assert_eq!(alpha_at(&dotted, 1, 1), alpha_at(&plain, 1, 1));
        assert_ne!(plain.rgba(), dotted.rgba());
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
        let plain = render(key(Glyph::OnDark, Variant::Normal, None)).expect("plain");
        let outlined = render(key(Glyph::Outlined, Variant::Normal, None)).expect("outlined");
        let (light, dark) = ([0xf5, 0xf5, 0xf7], [0x24, 0x24, 0x27]);
        // 白色字形在浅色面板上几乎看不见；描边之后有明显对比
        assert!(contrast_on(&plain, light) < 40.0, "{}", contrast_on(&plain, light));
        assert!(
            contrast_on(&outlined, light) > 140.0,
            "{}",
            contrast_on(&outlined, light)
        );
        // 深色面板上仍然清晰
        assert!(contrast_on(&outlined, dark) > 150.0);
        // 字形本身的像素保持不变、尺寸不变
        assert_eq!((outlined.width(), outlined.height()), (plain.width(), plain.height()));
        for (o, p) in outlined.rgba().chunks_exact(4).zip(plain.rgba().chunks_exact(4)) {
            if p[3] == 255 {
                assert_eq!(o, p);
            }
            assert!(o[3] >= p[3], "描边只增加不透明度");
        }
        // 状态图与圆点照常叠在描边版上
        for v in [Variant::Paused, Variant::Attention] {
            let state = render(key(Glyph::Outlined, v, None)).expect("state");
            assert_ne!(state.rgba(), outlined.rgba(), "{v:?}");
        }
        let flashing = render(key(Glyph::Outlined, Variant::Normal, Some(Dot::Flash))).expect("flash");
        assert_ne!(flashing.rgba(), outlined.rgba());
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
