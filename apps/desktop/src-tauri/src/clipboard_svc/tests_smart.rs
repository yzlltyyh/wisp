//! 智能识别在剪贴板服务里的接入：条目检视（结果与缓存）、收藏命名、验证码条目的到期删除（一次性排程、定时器晚到时
//! 读取时过滤与当场删除、收藏取消过期、打开数据库时补删）。

use std::sync::atomic::Ordering;

use wisp_clipboard::OTP_TTL_MS;
use wisp_types::{
    ClipFilter, ClipInspect, ClipboardChanged, EntityKind, OsKind, RawClipboard, SmartSettings, TransformId,
};

use crate::error::IpcErrorCode;

use super::smart::inspect_text;
use super::tests::{ICON_64, Rig, rig, rig_with};

/// 一条会被识别为验证码的消息
const CODE: &str = "Your verification code is 482915\nDo not share it.";

fn exists(r: &Rig, id: i64) -> bool {
    r.engine
        .ready_store()
        .expect("已打开")
        .text_of(id)
        .expect("查询")
        .is_some()
}

// ---------- 检视 ----------

#[test]
fn inspect_returns_entities_and_transforms_and_caches_by_content() {
    let r = rig();
    let a = r.copy_text(101, CODE);
    let ins = r.engine.inspect(a.id).expect("检视");
    assert_eq!(
        ins.entities.iter().map(|e| e.kind).collect::<Vec<_>>(),
        [EntityKind::Otp]
    );
    assert_eq!(ins.entities[0].value, "482915");
    assert!(ins.transforms.contains(&TransformId::JoinLines));
    assert_eq!(r.engine.inspect_cache.len(), 1);
    // 再看一次：命中缓存，结果相同
    assert_eq!(r.engine.inspect(a.id).expect("检视"), ins);
    assert_eq!(r.engine.inspect_cache.len(), 1);
    // 识别设置变了：旧结果作废，按新设置重算
    r.set_settings(|s| s.clipboard.smart.codes = false);
    let off = r.engine.inspect(a.id).expect("检视");
    assert!(off.entities.is_empty(), "{:?}", off.entities);
    assert_eq!(off.transforms, ins.transforms);
    // 不存在的条目
    let err = r.engine.inspect(9999).expect_err("不存在");
    assert_eq!(err.code, IpcErrorCode::NotFound);
    // 图片没有正文：结果为空
    r.copy(
        102,
        RawClipboard {
            change_id: 102,
            image_png: Some(ICON_64.to_vec()),
            ..RawClipboard::default()
        },
    );
    let img = r.top();
    assert_eq!(r.engine.inspect(img.id).expect("检视"), ClipInspect::default());
}

#[test]
fn plain_text_is_inspected_with_the_given_settings() {
    let text = "Call 13800138000 tomorrow";
    let on = inspect_text(text, &SmartSettings::default());
    assert_eq!(
        on.entities.iter().map(|e| e.kind).collect::<Vec<_>>(),
        [EntityKind::Phone]
    );
    let off = SmartSettings {
        contacts: false,
        ..SmartSettings::default()
    };
    assert!(inspect_text(text, &off).entities.is_empty());
}

// ---------- 收藏命名 ----------

#[test]
fn naming_favorites_the_item_and_clearing_keeps_the_favorite() {
    let r = rig();
    let a = r.copy_text(101, "Guest network: wisp-guest");
    r.copy_text(102, "newer");
    let named = r.engine.set_name(a.id, Some("Wi-Fi")).expect("命名");
    assert!(named.pinned);
    assert_eq!(named.name.as_deref(), Some("Wi-Fi"));
    // 缓存随之更新：首屏里它排在收藏区、带着名称
    let top = r.top();
    assert_eq!((top.id, top.name.as_deref()), (a.id, Some("Wi-Fi")));
    // 清除名称：仍是收藏
    let cleared = r.engine.set_name(a.id, None).expect("清除");
    assert!(cleared.pinned && cleared.name.is_none());
    // 取消收藏一并去掉名称
    r.engine.set_name(a.id, Some("again")).expect("命名");
    let unpinned = r.engine.pin(a.id, false).expect("取消收藏");
    assert!(!unpinned.pinned && unpinned.name.is_none());
    // 不合规的名称（命令层第一步已清理；这里是存储层的兜底）
    let err = r.engine.set_name(a.id, Some(&"x".repeat(65))).expect_err("太长");
    assert_eq!(err.code, IpcErrorCode::InvalidArgument);
}

// ---------- 验证码到期 ----------

#[test]
fn verification_codes_expire_on_a_one_shot_schedule() {
    let r = rig();
    let keep = r.copy_text(100, "keep me");
    let code = r.copy_text(101, CODE);
    let exp = code.expires_at.expect("验证码带过期时间");
    let now = r.host.now.load(Ordering::SeqCst);
    assert!(exp > now && exp - now <= OTP_TTL_MS, "{exp} vs {now}");
    assert!(keep.expires_at.is_none());
    r.events();
    // 定时器提前醒来（还没到点）：什么都不删，按同一个时刻再排一次
    assert_eq!(r.exec.fire_timers(), 1);
    assert!(exists(&r, code.id));
    // 到点：删除、推送 removed；没有剩下的验证码，不再排程
    r.host.now.store(exp + 1, Ordering::SeqCst);
    assert_eq!(r.exec.fire_timers(), 1);
    assert!(!exists(&r, code.id));
    assert_eq!(r.events(), vec![ClipboardChanged::Removed { id: code.id }]);
    assert_eq!(r.ids(), vec![keep.id]);
    assert_eq!(r.exec.fire_timers(), 0);
}

#[test]
fn expired_codes_never_show_even_when_the_timer_is_late() {
    let r = rig();
    let code = r.copy_text(101, CODE);
    let exp = code.expires_at.expect("过期时间");
    // 系统睡眠：定时器还没触发，时间已经过了
    r.host.now.store(exp + 10, Ordering::SeqCst);
    // 首屏（来自缓存）：过滤掉，并在后台当场删除
    assert!(!r.ids().contains(&code.id));
    assert!(!exists(&r, code.id));

    // 查库的分页：先删除到期的，再查（之后的偏移不含它们）
    let code = r.copy_text(102, "Your verification code is 771203");
    let exp = code.expires_at.expect("过期时间");
    r.host.now.store(exp + 10, Ordering::SeqCst);
    r.exec.queue(true);
    let page = r.engine.recent(0, 50, ClipFilter::All).expect("分页");
    assert!(page.items.iter().all(|h| h.item.id != code.id));
    assert!(!exists(&r, code.id), "查询之前已经删除，不必等后台任务");
    r.exec.queue(false);
    r.exec.run_jobs();
}

#[test]
fn favoriting_or_naming_a_code_cancels_its_expiry() {
    let r = rig();
    let a = r.copy_text(101, CODE);
    let b = r.copy_text(102, "Your verification code is 771203");
    let exp = a.expires_at.max(b.expires_at).expect("过期时间");
    assert!(r.engine.pin(a.id, true).expect("收藏").expires_at.is_none());
    assert!(
        r.engine
            .set_name(b.id, Some("bank"))
            .expect("命名")
            .expires_at
            .is_none()
    );
    r.host.now.store(exp + 1, Ordering::SeqCst);
    r.exec.fire_timers();
    assert!(exists(&r, a.id) && exists(&r, b.id));
    let ids = r.ids();
    assert!(ids.contains(&a.id) && ids.contains(&b.id));
    // 取消收藏后也不恢复过期时间
    assert!(r.engine.pin(a.id, false).expect("取消").expires_at.is_none());
}

#[test]
fn copying_a_code_again_does_not_extend_its_expiry() {
    let r = rig();
    let first = r.copy_text(101, CODE);
    let again = r.copy_text(102, CODE);
    assert_eq!(again.id, first.id);
    assert_eq!(again.use_count, 2);
    assert_eq!(again.expires_at, first.expires_at);
}

#[test]
fn codes_are_kept_when_auto_delete_is_off() {
    let r = rig();
    r.set_settings(|s| s.clipboard.smart.otp_auto_delete = false);
    let code = r.copy_text(101, CODE);
    assert!(code.expires_at.is_none());
    assert_eq!(r.exec.fire_timers(), 0, "没有要排的到期任务");
}

#[test]
fn codes_due_while_wisp_was_closed_go_when_the_store_opens() {
    let r = rig();
    let keep = r.copy_text(100, "keep me");
    let code = r.copy_text(101, CODE);
    let exp = code.expires_at.expect("过期时间");
    // 退出（数据库随 Engine 关闭），之后在到期之后重新启动
    let Rig { dir, .. } = r;
    let r = rig_with(OsKind::Macos, dir);
    r.host.now.store(exp + 1, Ordering::SeqCst);
    r.engine.preload();
    assert!(!exists(&r, code.id), "打开时补删");
    assert_eq!(r.ids(), vec![keep.id]);

    // 还没到期的：打开时排上一次性任务
    let fresh = r.copy_text(102, "Your verification code is 771203");
    let exp = fresh.expires_at.expect("过期时间");
    let Rig { dir, .. } = r;
    let r = rig_with(OsKind::Macos, dir);
    r.host.now.store(exp - 1_000, Ordering::SeqCst);
    r.engine.preload();
    assert!(exists(&r, fresh.id));
    r.host.now.store(exp + 1, Ordering::SeqCst);
    assert_eq!(r.exec.fire_timers(), 1);
    assert!(!exists(&r, fresh.id));
}

#[test]
fn failed_expiry_retries_later_instead_of_spinning() {
    use super::smart::{EXPIRY_RETRY_MS, next_expiry_run};
    let now = 1_000_000;
    // 成功：按下一个过期时间排
    assert_eq!(next_expiry_run(now + 5, now, false), now + 5);
    // 失败：最早的过期时间已经过去，至少推迟一分钟再试
    assert_eq!(next_expiry_run(now - 10, now, true), now + EXPIRY_RETRY_MS);
    assert_eq!(
        next_expiry_run(now + 2 * EXPIRY_RETRY_MS, now, true),
        now + 2 * EXPIRY_RETRY_MS
    );
}
