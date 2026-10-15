//! JSON Merge Patch（RFC 7396）：对象逐键合并；`null` 删除该键（随后由 serde default 补回默认值）；数组整体替换。

/// 把 `patch` 合并进 `target`
pub fn merge_patch(target: &mut serde_json::Value, patch: &serde_json::Value) {
    let serde_json::Value::Object(patch_map) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = serde_json::Value::Object(serde_json::Map::new());
    }
    if let serde_json::Value::Object(target_map) = target {
        for (key, value) in patch_map {
            if value.is_null() {
                target_map.remove(key);
            } else {
                merge_patch(target_map.entry(key.clone()).or_insert(serde_json::Value::Null), value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::merge_patch;

    /// RFC 7396 附录 A 的部分用例
    #[test]
    fn rfc7396_examples() {
        let cases = [
            (json!({"a":"b"}), json!({"a":"c"}), json!({"a":"c"})),
            (json!({"a":"b"}), json!({"b":"c"}), json!({"a":"b","b":"c"})),
            (json!({"a":"b"}), json!({"a":null}), json!({})),
            (json!({"a":["b"]}), json!({"a":"c"}), json!({"a":"c"})),
            (json!({"a":"c"}), json!({"a":["b"]}), json!({"a":["b"]})),
            (
                json!({"a":{"b":"c"}}),
                json!({"a":{"b":"d","c":null}}),
                json!({"a":{"b":"d"}}),
            ),
            (json!({"a":[{"b":"c"}]}), json!({"a":[1]}), json!({"a":[1]})),
            (json!({"e":null}), json!({"a":1}), json!({"e":null,"a":1})),
            (json!([1, 2]), json!({"a":"b","c":null}), json!({"a":"b"})),
        ];
        for (mut target, patch, expected) in cases {
            merge_patch(&mut target, &patch);
            assert_eq!(target, expected);
        }
    }
}
