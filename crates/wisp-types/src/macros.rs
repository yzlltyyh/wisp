//! crate 内部的声明宏。

/// 声明一个以字符串序列化的无数据枚举，每个变体写成 `Variant = "wire_name"`。
///
/// 宏从同一份变体列表生成三样东西：枚举本身（每个变体带 `#[serde(rename = "wire_name")]`，`Serialize` /
/// `Deserialize` 仍由调用处派生）、按声明顺序列出全部变体的 `ALL`，以及与 serde 一致的 `as_str`。手写时这三处各
/// 自维护：新增变体后 `ALL` 漏项编译器不会提醒，`as_str` 的字符串也可能与 serde 的改名规则悄悄不一致。集中成一份
/// 列表后加变体只改一处，前端用来比对的常量清单（`fixtures/consts/`）也由 `ALL` 生成，不会漏掉新变体。
///
/// 声明顺序即 `ALL` 的顺序；顺序有界面含义时（菜单、设置页）写在枚举自己的文档里。
macro_rules! str_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$vmeta:meta])*
                $variant:ident = $wire:literal
            ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis enum $name {
            $(
                $(#[$vmeta])*
                #[serde(rename = $wire)]
                $variant,
            )+
        }

        impl $name {
            /// 全部取值（声明顺序）
            pub const ALL: [Self; [$($wire),+].len()] = [$(Self::$variant),+];

            /// 序列化后的字符串（与 serde 一致）
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire,)+
                }
            }
        }
    };
}
pub(crate) use str_enum;
