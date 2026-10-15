// 本文件由 crates/wisp-types/tests/golden.rs 生成，请勿手改。
// 重新生成：UPDATE_GOLDEN=1 cargo test -p wisp-types --test golden
// 每个常量都用 `satisfies` 获得上下文类型，字面量不会被拓宽。
import type * as T from '../types'

export const Point = {
  "x": 120.5,
  "y": 48.0
} satisfies T.Point

export const Size = {
  "width": 420.0,
  "height": 168.0
} satisfies T.Size

export const Rect = {
  "x": 412.5,
  "y": 300.0,
  "width": 180.0,
  "height": 18.0
} satisfies T.Rect

export const PhysPoint = {
  "x": -1920,
  "y": 0
} satisfies T.PhysPoint

export const PhysSize = {
  "width": 3024,
  "height": 1964
} satisfies T.PhysSize

export const MonitorInfo = {
  "id": "1",
  "name": "Built-in Retina Display",
  "physical_origin": {
    "x": 0,
    "y": 0
  },
  "physical_size": {
    "width": 3024,
    "height": 1964
  },
  "scale_factor": 2.0,
  "frame": {
    "x": 0.0,
    "y": 0.0,
    "width": 1512.0,
    "height": 982.0
  },
  "work_area": {
    "x": 0.0,
    "y": 33.0,
    "width": 1512.0,
    "height": 949.0
  },
  "is_primary": true
} satisfies T.MonitorInfo

export const Anchor = {
  "monitor_id": "1",
  "rect": {
    "x": 412.5,
    "y": 300.0,
    "width": 180.0,
    "height": 18.0
  }
} satisfies T.Anchor

export const AppRef = {
  "id": "com.apple.Safari",
  "alt_id": null,
  "name": "Safari",
  "pid": 4242,
  "path": "/Applications/Safari.app",
  "window": null
} satisfies T.AppRef

export const Capability = {
  "available": false,
  "reason_key": "errors.platform.wayland"
} satisfies T.Capability

export const Capabilities = {
  "global_hotkeys": {
    "available": true,
    "reason_key": null
  },
  "selection": {
    "available": true,
    "reason_key": null
  },
  "simulated_copy": {
    "available": true,
    "reason_key": null
  },
  "paste_inject": {
    "available": true,
    "reason_key": null
  },
  "capture_overlay": {
    "available": true,
    "reason_key": null
  },
  "clipboard_watch": {
    "available": true,
    "reason_key": null
  },
  "input_hooks": {
    "available": false,
    "reason_key": "errors.platform.input_monitoring_missing"
  }
} satisfies T.Capabilities

export const PlatformInfo = {
  "os": "macos",
  "os_version": "26.4",
  "arch": "aarch64",
  "session": "native",
  "capabilities": {
    "global_hotkeys": {
      "available": true,
      "reason_key": null
    },
    "selection": {
      "available": true,
      "reason_key": null
    },
    "simulated_copy": {
      "available": true,
      "reason_key": null
    },
    "paste_inject": {
      "available": true,
      "reason_key": null
    },
    "capture_overlay": {
      "available": true,
      "reason_key": null
    },
    "clipboard_watch": {
      "available": true,
      "reason_key": null
    },
    "input_hooks": {
      "available": false,
      "reason_key": "errors.platform.input_monitoring_missing"
    }
  }
} satisfies T.PlatformInfo

export const Quirks = {
  "thinking_off": "openai_effort",
  "merge_system_into_user": false,
  "strip_think_tags": true,
  "max_tokens_field": "max_completion_tokens",
  "dual_auth_header": null,
  "gemini_key_in_query": false
} satisfies T.Quirks

export const ModelCaps = {
  "temperature": false,
  "max_tokens_field": "max_completion_tokens",
  "thinking_param": null,
  "effort_param": true,
  "stream_options": true,
  "include_obfuscation": null,
  "anthropic_thinking_disabled": null,
  "anthropic_efforts": null
} satisfies T.ModelCaps

export const ModelEntry = {
  "id": "gpt-5-mini",
  "label": "GPT-5 mini",
  "vision": "yes",
  "vision_source": "keyword",
  "reasoning": true,
  "enabled": true,
  "missing_upstream": false,
  "deprecated": false,
  "context_tokens": 400000,
  "caps": {
    "temperature": false,
    "max_tokens_field": "max_completion_tokens",
    "thinking_param": null,
    "effort_param": true,
    "stream_options": true,
    "include_obfuscation": null,
    "anthropic_thinking_disabled": null,
    "anthropic_efforts": null
  },
  "reasoning_effort": "minimal"
} satisfies T.ModelEntry

export const Timeouts = {
  "connect_ms": 10000,
  "first_token_ms": 120000,
  "idle_ms": 120000
} satisfies T.Timeouts

export const Provider = {
  "id": "openai-main",
  "name": "OpenAI",
  "protocol": "openai_chat",
  "base_url": "https://api.openai.com/v1",
  "headers": {
    "x-org": "wisp"
  },
  "quirks": {
    "thinking_off": "openai_effort",
    "merge_system_into_user": false,
    "strip_think_tags": true,
    "max_tokens_field": "max_completion_tokens",
    "dual_auth_header": null,
    "gemini_key_in_query": false
  },
  "models": [
    {
      "id": "gpt-5-mini",
      "label": "GPT-5 mini",
      "vision": "yes",
      "vision_source": "keyword",
      "reasoning": true,
      "enabled": true,
      "missing_upstream": false,
      "deprecated": false,
      "context_tokens": 400000,
      "caps": {
        "temperature": false,
        "max_tokens_field": "max_completion_tokens",
        "thinking_param": null,
        "effort_param": true,
        "stream_options": true,
        "include_obfuscation": null,
        "anthropic_thinking_disabled": null,
        "anthropic_efforts": null
      },
      "reasoning_effort": "minimal"
    },
    {
      "id": "gpt-4.1-nano",
      "label": null,
      "vision": "unknown",
      "vision_source": null,
      "reasoning": false,
      "enabled": true,
      "missing_upstream": false,
      "deprecated": false,
      "context_tokens": null,
      "caps": {
        "temperature": null,
        "max_tokens_field": null,
        "thinking_param": null,
        "effort_param": null,
        "stream_options": null,
        "include_obfuscation": null,
        "anthropic_thinking_disabled": null,
        "anthropic_efforts": null
      },
      "reasoning_effort": null
    }
  ],
  "timeouts": {
    "connect_ms": 10000,
    "first_token_ms": 30000,
    "idle_ms": 60000
  },
  "template_id": "openai"
} satisfies T.Provider

export const ModelRef = {
  "provider_id": "openai-main",
  "model_id": "gpt-5-mini"
} satisfies T.ModelRef

export const Defaults = {
  "translate": {
    "provider_id": "openai-main",
    "model_id": "gpt-5-mini"
  },
  "ocr": null
} satisfies T.Defaults

export const Usage = {
  "input_tokens": 412,
  "output_tokens": 96,
  "reasoning_tokens": 0,
  "total_tokens": 508
} satisfies T.Usage

export const LlmError = {
  "code": "rate_limited",
  "subcode": null,
  "message": "Rate limit reached for gpt-5-mini",
  "retryable": true,
  "hint": "errors.hint.retry_later",
  "status": 429,
  "url": "api.openai.com/v1/chat/completions",
  "retry_after_s": 20,
  "upstream_request_id": "req_8f2c1a"
} satisfies T.LlmError

export const StreamEvent__start = {
  "type": "start",
  "request_id": "r-1",
  "provider_id": "openai-main",
  "model_id": "gpt-5-mini",
  "model_label": "GPT-5 mini",
  "upstream_id": "chatcmpl-9x"
} satisfies T.StreamEvent

export const StreamEvent__meta = {
  "type": "meta",
  "detected_source_lang": "en",
  "target_lang": "zh-CN",
  "dict_mode": false,
  "low_confidence": false
} satisfies T.StreamEvent

export const StreamEvent__phase = {
  "type": "phase",
  "phase": "reviewing"
} satisfies T.StreamEvent

export const StreamEvent__delta = {
  "type": "delta",
  "text": "敏捷的棕色狐狸"
} satisfies T.StreamEvent

export const StreamEvent__notice = {
  "type": "notice",
  "kind": "param_degraded"
} satisfies T.StreamEvent

export const StreamEvent__done = {
  "type": "done",
  "elapsed_ms": 1240,
  "usage": {
    "input_tokens": 412,
    "output_tokens": 96,
    "reasoning_tokens": 0,
    "total_tokens": 508
  },
  "truncated": "max_tokens",
  "cached": false,
  "no_text": false
} satisfies T.StreamEvent

export const StreamEvent__error = {
  "type": "error",
  "code": "rate_limited",
  "subcode": null,
  "message": "Rate limit reached for gpt-5-mini",
  "retryable": true,
  "hint": "errors.hint.retry_later",
  "status": 429,
  "url": "api.openai.com/v1/chat/completions",
  "retry_after_s": 20,
  "upstream_request_id": "req_8f2c1a"
} satisfies T.StreamEvent

export const RegionVariant = {
  "label_key": "settings.providers.region.intl",
  "base_url": "https://api.deepseek.com"
} satisfies T.RegionVariant

export const ProviderTemplate = {
  "id": "openai",
  "name_key": "settings.providers.template.openai",
  "group": "openai",
  "provider": {
    "id": "openai-main",
    "name": "OpenAI",
    "protocol": "openai_chat",
    "base_url": "https://api.openai.com/v1",
    "headers": {
      "x-org": "wisp"
    },
    "quirks": {
      "thinking_off": "openai_effort",
      "merge_system_into_user": false,
      "strip_think_tags": true,
      "max_tokens_field": "max_completion_tokens",
      "dual_auth_header": null,
      "gemini_key_in_query": false
    },
    "models": [
      {
        "id": "gpt-5-mini",
        "label": "GPT-5 mini",
        "vision": "yes",
        "vision_source": "keyword",
        "reasoning": true,
        "enabled": true,
        "missing_upstream": false,
        "deprecated": false,
        "context_tokens": 400000,
        "caps": {
          "temperature": false,
          "max_tokens_field": "max_completion_tokens",
          "thinking_param": null,
          "effort_param": true,
          "stream_options": true,
          "include_obfuscation": null,
          "anthropic_thinking_disabled": null,
          "anthropic_efforts": null
        },
        "reasoning_effort": "minimal"
      },
      {
        "id": "gpt-4.1-nano",
        "label": null,
        "vision": "unknown",
        "vision_source": null,
        "reasoning": false,
        "enabled": true,
        "missing_upstream": false,
        "deprecated": false,
        "context_tokens": null,
        "caps": {
          "temperature": null,
          "max_tokens_field": null,
          "thinking_param": null,
          "effort_param": null,
          "stream_options": null,
          "include_obfuscation": null,
          "anthropic_thinking_disabled": null,
          "anthropic_efforts": null
        },
        "reasoning_effort": null
      }
    ],
    "timeouts": {
      "connect_ms": 10000,
      "first_token_ms": 30000,
      "idle_ms": 60000
    },
    "template_id": "openai"
  },
  "key_required": true,
  "region_variants": [
    {
      "label_key": "settings.providers.region.default",
      "base_url": "https://api.openai.com/v1"
    }
  ]
} satisfies T.ProviderTemplate

export const UrlPreview = {
  "method": "POST",
  "url": "https://api.openai.com/v1/chat/completions",
  "version_appended": false,
  "forced": false
} satisfies T.UrlPreview

export const ProbeResult = {
  "ok": false,
  "status": 401,
  "elapsed_ms": 312,
  "url": "api.openai.com/v1/models",
  "error": {
    "code": "auth",
    "subcode": null,
    "message": "Incorrect API key provided",
    "retryable": false,
    "hint": null,
    "status": 401,
    "url": null,
    "retry_after_s": null,
    "upstream_request_id": null
  }
} satisfies T.ProbeResult

export const ProviderTestReport = {
  "models": {
    "ok": true,
    "status": 200,
    "elapsed_ms": 280,
    "url": "api.openai.com/v1/models",
    "error": null
  },
  "stream": {
    "ok": true,
    "status": 200,
    "elapsed_ms": 640,
    "url": "api.openai.com/v1/chat/completions",
    "error": null
  },
  "overall": "ok",
  "model_used": "gpt-5-mini",
  "fetched_models": [
    {
      "id": "gpt-5-mini",
      "label": "GPT-5 mini",
      "vision": "yes",
      "vision_source": "keyword",
      "reasoning": true,
      "enabled": true,
      "missing_upstream": false,
      "deprecated": false,
      "context_tokens": 400000,
      "caps": {
        "temperature": false,
        "max_tokens_field": "max_completion_tokens",
        "thinking_param": null,
        "effort_param": true,
        "stream_options": true,
        "include_obfuscation": null,
        "anthropic_thinking_disabled": null,
        "anthropic_efforts": null
      },
      "reasoning_effort": "minimal"
    }
  ]
} satisfies T.ProviderTestReport

export const Style = {
  "id": "custom-3f9a1c2e",
  "name": "Release notes",
  "builtin": false,
  "system_template": "Translate release notes from {{source_lang}} into {{target_lang}}.",
  "tail_instruction": "Reply with the translation only.",
  "temperature": null,
  "output": "markdown_lite",
  "dictionary_mode": false,
  "model_ref": {
    "provider_id": "openai-main",
    "model_id": "gpt-5-mini"
  },
  "hotkey": "Control+Alt+KeyR",
  "modified": false
} satisfies T.Style

export const GlossaryEntry = {
  "source": "Wisp",
  "target": "Wisp",
  "note": "产品名，不译"
} satisfies T.GlossaryEntry

export const PromptPreview = {
  "system": "You are a professional translator.",
  "user": "<source_text>\nhi\n</source_text>"
} satisfies T.PromptPreview

export const SourceApp = {
  "id": "com.apple.Safari",
  "name": "Safari",
  "icon_url": "http://wisp.localhost/icon%2Fa1b2c3.png"
} satisfies T.SourceApp

export const ImageMeta = {
  "width": 1280,
  "height": 720,
  "bytes": 245760,
  "thumb_url": null
} satisfies T.ImageMeta

export const FileRef = {
  "path": "/Users/me/Desktop/report.pdf",
  "name": "report.pdf",
  "exists": true,
  "size": 182044
} satisfies T.FileRef

export const ClipItemMeta = {
  "id": 129,
  "kind": "image",
  "created_at": 1760000100000,
  "last_used_at": 1760000100000,
  "use_count": 1,
  "pinned": true,
  "source": null,
  "preview": "",
  "char_count": null,
  "line_count": null,
  "truncated": false,
  "image": {
    "width": 1280,
    "height": 720,
    "bytes": 245760,
    "thumb_url": "http://wisp.localhost/thumb%2Fd4e5f6.jpg"
  },
  "files": [
    {
      "path": "/Users/me/Desktop/report.pdf",
      "name": "report.pdf",
      "exists": true,
      "size": 182044
    }
  ],
  "link": "https://example.com/",
  "color": "#3478F6"
} satisfies T.ClipItemMeta

export const HighlightRange = {
  "start": 4,
  "end": 9
} satisfies T.HighlightRange

export const ClipHit = {
  "item": {
    "id": 128,
    "kind": "text",
    "created_at": 1760000000000,
    "last_used_at": 1760000360000,
    "use_count": 3,
    "pinned": false,
    "source": {
      "id": "com.apple.Safari",
      "name": "Safari",
      "icon_url": "http://wisp.localhost/icon%2Fa1b2c3.png"
    },
    "preview": "The quick brown fox jumps over the lazy dog",
    "char_count": 43,
    "line_count": 1,
    "truncated": false,
    "image": null,
    "files": null,
    "link": null,
    "color": null
  },
  "highlights": [
    {
      "start": 4,
      "end": 9
    }
  ]
} satisfies T.ClipHit

export const ClipPage = {
  "items": [
    {
      "item": {
        "id": 128,
        "kind": "text",
        "created_at": 1760000000000,
        "last_used_at": 1760000360000,
        "use_count": 3,
        "pinned": false,
        "source": {
          "id": "com.apple.Safari",
          "name": "Safari",
          "icon_url": "http://wisp.localhost/icon%2Fa1b2c3.png"
        },
        "preview": "The quick brown fox jumps over the lazy dog",
        "char_count": 43,
        "line_count": 1,
        "truncated": false,
        "image": null,
        "files": null,
        "link": null,
        "color": null
      },
      "highlights": [
        {
          "start": 4,
          "end": 9
        }
      ]
    },
    {
      "item": {
        "id": 129,
        "kind": "image",
        "created_at": 1760000100000,
        "last_used_at": 1760000100000,
        "use_count": 1,
        "pinned": true,
        "source": null,
        "preview": "",
        "char_count": null,
        "line_count": null,
        "truncated": false,
        "image": {
          "width": 1280,
          "height": 720,
          "bytes": 245760,
          "thumb_url": "http://wisp.localhost/thumb%2Fd4e5f6.jpg"
        },
        "files": [
          {
            "path": "/Users/me/Desktop/report.pdf",
            "name": "report.pdf",
            "exists": true,
            "size": 182044
          }
        ],
        "link": "https://example.com/",
        "color": "#3478F6"
      },
      "highlights": []
    }
  ],
  "pinned_count": 1,
  "next_offset": 50,
  "query": "quick",
  "filter": "all"
} satisfies T.ClipPage

export const ClipItemDetail = {
  "meta": {
    "id": 128,
    "kind": "text",
    "created_at": 1760000000000,
    "last_used_at": 1760000360000,
    "use_count": 3,
    "pinned": false,
    "source": {
      "id": "com.apple.Safari",
      "name": "Safari",
      "icon_url": "http://wisp.localhost/icon%2Fa1b2c3.png"
    },
    "preview": "The quick brown fox jumps over the lazy dog",
    "char_count": 43,
    "line_count": 1,
    "truncated": false,
    "image": null,
    "files": null,
    "link": null,
    "color": null
  },
  "text": "The quick brown fox jumps over the lazy dog",
  "has_html": true,
  "has_rtf": false,
  "image_url": null,
  "files": null
} satisfies T.ClipItemDetail

export const PasteOutcome__pasted = {
  "type": "pasted"
} satisfies T.PasteOutcome

export const PasteOutcome__copied = {
  "type": "copied",
  "reason": "elevated"
} satisfies T.PasteOutcome

export const ClipboardChanged__inserted = {
  "change": "inserted",
  "item": {
    "id": 128,
    "kind": "text",
    "created_at": 1760000000000,
    "last_used_at": 1760000360000,
    "use_count": 3,
    "pinned": false,
    "source": {
      "id": "com.apple.Safari",
      "name": "Safari",
      "icon_url": "http://wisp.localhost/icon%2Fa1b2c3.png"
    },
    "preview": "The quick brown fox jumps over the lazy dog",
    "char_count": 43,
    "line_count": 1,
    "truncated": false,
    "image": null,
    "files": null,
    "link": null,
    "color": null
  }
} satisfies T.ClipboardChanged

export const ClipboardChanged__touched = {
  "change": "touched",
  "item": {
    "id": 129,
    "kind": "image",
    "created_at": 1760000100000,
    "last_used_at": 1760000100000,
    "use_count": 1,
    "pinned": true,
    "source": null,
    "preview": "",
    "char_count": null,
    "line_count": null,
    "truncated": false,
    "image": {
      "width": 1280,
      "height": 720,
      "bytes": 245760,
      "thumb_url": "http://wisp.localhost/thumb%2Fd4e5f6.jpg"
    },
    "files": [
      {
        "path": "/Users/me/Desktop/report.pdf",
        "name": "report.pdf",
        "exists": true,
        "size": 182044
      }
    ],
    "link": "https://example.com/",
    "color": "#3478F6"
  }
} satisfies T.ClipboardChanged

export const ClipboardChanged__updated = {
  "change": "updated",
  "item": {
    "id": 129,
    "kind": "image",
    "created_at": 1760000100000,
    "last_used_at": 1760000100000,
    "use_count": 1,
    "pinned": true,
    "source": null,
    "preview": "",
    "char_count": null,
    "line_count": null,
    "truncated": false,
    "image": {
      "width": 1280,
      "height": 720,
      "bytes": 245760,
      "thumb_url": "http://wisp.localhost/thumb%2Fd4e5f6.jpg"
    },
    "files": [
      {
        "path": "/Users/me/Desktop/report.pdf",
        "name": "report.pdf",
        "exists": true,
        "size": 182044
      }
    ],
    "link": "https://example.com/",
    "color": "#3478F6"
  }
} satisfies T.ClipboardChanged

export const ClipboardChanged__removed = {
  "change": "removed",
  "id": 128
} satisfies T.ClipboardChanged

export const ClipboardChanged__cleared = {
  "change": "cleared",
  "keep_pinned": true
} satisfies T.ClipboardChanged

export const ClipboardChanged__paused = {
  "change": "paused",
  "paused": true
} satisfies T.ClipboardChanged

export const ClipboardChanged__secret_skipped = {
  "change": "secret_skipped"
} satisfies T.ClipboardChanged

export const OcrHistoryItem = {
  "id": 5,
  "created_at": 1760000000000,
  "mode": "markdown",
  "model": "gpt-5-mini",
  "text": "# Title\n\nBody",
  "thumb_url": "http://wisp.localhost/ocrimg%2F5.jpg",
  "image_url": "http://wisp.localhost/ocrimg%2F5.png"
} satisfies T.OcrHistoryItem

export const OcrHistoryChanged__inserted = {
  "change": "inserted",
  "item": {
    "id": 6,
    "created_at": 1760000100000,
    "mode": "lines",
    "model": "GPT-5 mini",
    "text": "Hello",
    "thumb_url": null,
    "image_url": null
  }
} satisfies T.OcrHistoryChanged

export const OcrHistoryChanged__removed = {
  "change": "removed",
  "ids": [
    3,
    4
  ]
} satisfies T.OcrHistoryChanged

export const OcrHistoryChanged__cleared = {
  "change": "cleared"
} satisfies T.OcrHistoryChanged

export const HotkeyTarget__action = {
  "kind": "action",
  "action": "translate_selection"
} satisfies T.HotkeyTarget

export const HotkeyTarget__style = {
  "kind": "style",
  "style_id": "technical"
} satisfies T.HotkeyTarget

export const Conflict = {
  "level": "warning",
  "source": "static_app",
  "message_key": "hotkeys.conflict.static_app",
  "params": {
    "app": "Rectangle"
  }
} satisfies T.Conflict

export const ConflictReport = {
  "combo": "Control+Alt+KeyD",
  "display": "⌃⌥D",
  "conflicts": [
    {
      "level": "warning",
      "source": "static_app",
      "message_key": "hotkeys.conflict.static_app",
      "params": {
        "app": "Rectangle"
      }
    }
  ],
  "acceptable": true
} satisfies T.ConflictReport

export const HotkeyStatus = {
  "target": {
    "kind": "action",
    "action": "translate_selection"
  },
  "binding": "default",
  "effective": "Alt+KeyD",
  "display": "⌥D",
  "state": "registered",
  "reason_key": null
} satisfies T.HotkeyStatus

export const ExtTriggerStatus = {
  "bobber": "off",
  "double_tap": "waiting_permission",
  "mouse": "off",
  "reason_key": "hotkeys.ext.need_input_monitoring"
} satisfies T.ExtTriggerStatus

export const NeighborHit = {
  "app_name": "Bob",
  "combo": "Alt+KeyD",
  "display": "⌥D",
  "action": "translate_selection"
} satisfies T.NeighborHit

export const KeyboardAdvice = {
  "has_altgr": true,
  "multi_input_lang": false,
  "recommend_family": "alt_shift",
  "qq_running": false
} satisfies T.KeyboardAdvice

export const PermissionSet = {
  "accessibility": "granted",
  "screen_recording": "not_determined",
  "input_monitoring": "denied",
  "lost_after_update": false
} satisfies T.PermissionSet

export const PanelPrepare = {
  "session_id": 7,
  "view": "translate",
  "payload": {
    "kind": "translate",
    "source_text": "The quick brown fox jumps over the lazy dog.",
    "request_id": "r-1",
    "task": "translate",
    "style_id": "natural",
    "detected_lang": "en",
    "target_lang": "zh-CN",
    "dict_mode": false,
    "long_text": null,
    "not_ready": null,
    "selection_method": "ax"
  },
  "width": 420.0,
  "max_height": 569.4,
  "placement": "below",
  "origin_x": 24.0,
  "focused": false,
  "transition": "enter"
} satisfies T.PanelPrepare

export const PanelPayload__translate = {
  "kind": "translate",
  "source_text": "The quick brown fox jumps over the lazy dog.",
  "request_id": "r-1",
  "task": "translate",
  "style_id": "natural",
  "detected_lang": "en",
  "target_lang": "zh-CN",
  "dict_mode": false,
  "long_text": null,
  "not_ready": null,
  "selection_method": "ax"
} satisfies T.PanelPayload

export const PanelPayload__ocr = {
  "kind": "ocr",
  "capture_id": "c-7",
  "request_id": "r-2",
  "mode": "lines",
  "thumb_url": "http://wisp.localhost/crop%2Fc-7.png",
  "image_size": {
    "width": 840,
    "height": 312
  },
  "target_lang": "zh-CN",
  "not_ready": null
} satisfies T.PanelPayload

export const PanelPayload__clipboard = {
  "kind": "clipboard",
  "page": {
    "items": [
      {
        "item": {
          "id": 128,
          "kind": "text",
          "created_at": 1760000000000,
          "last_used_at": 1760000360000,
          "use_count": 3,
          "pinned": false,
          "source": {
            "id": "com.apple.Safari",
            "name": "Safari",
            "icon_url": "http://wisp.localhost/icon%2Fa1b2c3.png"
          },
          "preview": "The quick brown fox jumps over the lazy dog",
          "char_count": 43,
          "line_count": 1,
          "truncated": false,
          "image": null,
          "files": null,
          "link": null,
          "color": null
        },
        "highlights": [
          {
            "start": 4,
            "end": 9
          }
        ]
      },
      {
        "item": {
          "id": 129,
          "kind": "image",
          "created_at": 1760000100000,
          "last_used_at": 1760000100000,
          "use_count": 1,
          "pinned": true,
          "source": null,
          "preview": "",
          "char_count": null,
          "line_count": null,
          "truncated": false,
          "image": {
            "width": 1280,
            "height": 720,
            "bytes": 245760,
            "thumb_url": "http://wisp.localhost/thumb%2Fd4e5f6.jpg"
          },
          "files": [
            {
              "path": "/Users/me/Desktop/report.pdf",
              "name": "report.pdf",
              "exists": true,
              "size": 182044
            }
          ],
          "link": "https://example.com/",
          "color": "#3478F6"
        },
        "highlights": []
      }
    ],
    "pinned_count": 1,
    "next_offset": 50,
    "query": "quick",
    "filter": "all"
  },
  "paused": false,
  "hint_bar": true,
  "paste_on_double_click": false
} satisfies T.PanelPayload

export const PanelPayload__input = {
  "kind": "input",
  "initial_text": "",
  "target_lang": null,
  "hint": {
    "reason": "no_provider"
  }
} satisfies T.PanelPayload

export const PanelPayload__toast = {
  "kind": "toast",
  "reason": "focus_timeout",
  "paste_key": "⌘V"
} satisfies T.PanelPayload

export const ToastPayload = {
  "reason": "focus_timeout",
  "paste_key": "⌘V"
} satisfies T.ToastPayload

export const TranslatePayload = {
  "source_text": "The quick brown fox jumps over the lazy dog.",
  "request_id": null,
  "task": "translate",
  "style_id": "natural",
  "detected_lang": "en",
  "target_lang": "zh-CN",
  "dict_mode": false,
  "long_text": {
    "chars": 5200,
    "est_tokens": 1300,
    "est_seconds": 12.5
  },
  "not_ready": {
    "reason": "no_default_model",
    "task": "translate"
  },
  "selection_method": null
} satisfies T.TranslatePayload

export const LongTextInfo = {
  "chars": 5200,
  "est_tokens": 1300,
  "est_seconds": 12.5
} satisfies T.LongTextInfo

export const NotReady__no_provider = {
  "reason": "no_provider"
} satisfies T.NotReady

export const NotReady__no_default_model = {
  "reason": "no_default_model",
  "task": "polish"
} satisfies T.NotReady

export const NotReady__no_vision_model = {
  "reason": "no_vision_model"
} satisfies T.NotReady

export const NotReady__permission_missing = {
  "reason": "permission_missing",
  "permission": "screen_recording"
} satisfies T.NotReady

export const NotReady__wayland_copy_first = {
  "reason": "wayland_copy_first"
} satisfies T.NotReady

export const NotReady__capture_failed = {
  "reason": "capture_failed",
  "message": "errors.capture.backend"
} satisfies T.NotReady

export const NotReady__elevated = {
  "reason": "elevated"
} satisfies T.NotReady

export const NotReady__keychain_unavailable = {
  "reason": "keychain_unavailable"
} satisfies T.NotReady

export const NotReady__selection_unavailable = {
  "reason": "selection_unavailable",
  "reason_key": "errors.platform.clipboard_too_large"
} satisfies T.NotReady

export const OcrPayload = {
  "capture_id": "c-7",
  "request_id": "r-2",
  "mode": "lines",
  "thumb_url": "http://wisp.localhost/crop%2Fc-7.png",
  "image_size": {
    "width": 840,
    "height": 312
  },
  "target_lang": "zh-CN",
  "not_ready": null
} satisfies T.OcrPayload

export const ClipboardPayload = {
  "page": {
    "items": [
      {
        "item": {
          "id": 128,
          "kind": "text",
          "created_at": 1760000000000,
          "last_used_at": 1760000360000,
          "use_count": 3,
          "pinned": false,
          "source": {
            "id": "com.apple.Safari",
            "name": "Safari",
            "icon_url": "http://wisp.localhost/icon%2Fa1b2c3.png"
          },
          "preview": "The quick brown fox jumps over the lazy dog",
          "char_count": 43,
          "line_count": 1,
          "truncated": false,
          "image": null,
          "files": null,
          "link": null,
          "color": null
        },
        "highlights": [
          {
            "start": 4,
            "end": 9
          }
        ]
      },
      {
        "item": {
          "id": 129,
          "kind": "image",
          "created_at": 1760000100000,
          "last_used_at": 1760000100000,
          "use_count": 1,
          "pinned": true,
          "source": null,
          "preview": "",
          "char_count": null,
          "line_count": null,
          "truncated": false,
          "image": {
            "width": 1280,
            "height": 720,
            "bytes": 245760,
            "thumb_url": "http://wisp.localhost/thumb%2Fd4e5f6.jpg"
          },
          "files": [
            {
              "path": "/Users/me/Desktop/report.pdf",
              "name": "report.pdf",
              "exists": true,
              "size": 182044
            }
          ],
          "link": "https://example.com/",
          "color": "#3478F6"
        },
        "highlights": []
      }
    ],
    "pinned_count": 1,
    "next_offset": 50,
    "query": "quick",
    "filter": "all"
  },
  "paused": false,
  "hint_bar": true,
  "paste_on_double_click": false
} satisfies T.ClipboardPayload

export const InputPayload = {
  "initial_text": "",
  "target_lang": null,
  "hint": {
    "reason": "no_provider"
  }
} satisfies T.InputPayload

export const PanelShown = {
  "session_id": 7,
  "focused": false
} satisfies T.PanelShown

export const PanelHideRequest = {
  "session_id": 7,
  "reason": "click_outside"
} satisfies T.PanelHideRequest

export const PanelFocusChanged = {
  "session_id": 7,
  "focused": true
} satisfies T.PanelFocusChanged

export const LatencyMarks = {
  "session_id": 7,
  "origin": "hotkey",
  "method": "ax",
  "selection_ms": 18,
  "prepare_ms": 22,
  "ready_ms": 31,
  "shown_ms": 48,
  "first_token_ms": 420,
  "ready_timeout": false
} satisfies T.LatencyMarks

export const TranslateReq = {
  "session_id": 7,
  "text": "The quick brown fox",
  "task": "refine",
  "style_id": "natural",
  "target_lang": "zh-CN",
  "model_ref": {
    "provider_id": "openai-main",
    "model_id": "gpt-5-mini"
  },
  "dict_mode": "auto",
  "bypass_cache": true,
  "draft": "敏捷的棕色狐狸",
  "confirm_long": false
} satisfies T.TranslateReq

export const OcrReq = {
  "session_id": 9,
  "capture_id": "c-7",
  "mode": "translate",
  "model_ref": null,
  "target_lang": "zh-CN"
} satisfies T.OcrReq

export const StartAck = {
  "request_id": "r-3",
  "width": 480.0,
  "long_text": null
} satisfies T.StartAck

export const StartAck__long_text = {
  "request_id": "",
  "width": 480.0,
  "long_text": {
    "chars": 5200,
    "est_tokens": 1300,
    "est_seconds": 12.5
  }
} satisfies T.StartAck

export const PinState = {
  "kind": "translate",
  "source_text": "The quick brown fox jumps over the lazy dog.",
  "result_text": "敏捷的棕色狐狸跳过了懒狗。",
  "task": "translate",
  "style_id": "natural",
  "target_lang": "zh-CN",
  "detected_lang": "en",
  "request_id": null,
  "capture_id": null,
  "ocr_text": null,
  "ocr_mode": null,
  "show_source": true,
  "status_line": "gpt-5-mini · 1.2 s"
} satisfies T.PinState

export const PinState__ocr = {
  "kind": "ocr",
  "source_text": "",
  "result_text": "敏捷的棕色狐狸跳过了懒狗。",
  "task": "translate",
  "style_id": "natural",
  "target_lang": "zh-CN",
  "detected_lang": "en",
  "request_id": "r-9",
  "capture_id": "c-9",
  "ocr_text": "The quick brown fox jumps over the lazy dog.",
  "ocr_mode": "translate",
  "show_source": false,
  "status_line": null,
  "dict_mode": true,
  "model_ref": {
    "provider_id": "openai-main",
    "model_id": "gpt-5-mini"
  },
  "status": {
    "model_label": "GPT-5 mini",
    "elapsed_ms": null,
    "usage": null,
    "cached": true,
    "cancelled": false,
    "truncated": null
  },
  "thumb_url": "http://wisp.localhost/capture%2F9%2Fthumb",
  "image_size": {
    "width": 1280,
    "height": 720
  },
  "expand_width": 640,
  "on_top": false
} satisfies T.PinState

export const PinStatus = {
  "model_label": "GPT-5 mini",
  "elapsed_ms": 1240,
  "usage": {
    "input_tokens": 412,
    "output_tokens": 96,
    "reasoning_tokens": 0,
    "total_tokens": 508
  },
  "cached": false,
  "cancelled": false,
  "truncated": "max_tokens"
} satisfies T.PinStatus

export const WindowTarget__settings = {
  "kind": "settings",
  "section": "providers"
} satisfies T.WindowTarget

export const WindowTarget__settings_anchor = {
  "kind": "settings",
  "section": "ocr",
  "anchor": "ocr_history"
} satisfies T.WindowTarget

export const WindowTarget__onboarding = {
  "kind": "onboarding",
  "step": 2
} satisfies T.WindowTarget

export const OverlayInit = {
  "session_id": 3,
  "monitor": {
    "id": "1",
    "name": "Built-in Retina Display",
    "physical_origin": {
      "x": 0,
      "y": 0
    },
    "physical_size": {
      "width": 3024,
      "height": 1964
    },
    "scale_factor": 2.0,
    "frame": {
      "x": 0.0,
      "y": 0.0,
      "width": 1512.0,
      "height": 982.0
    },
    "work_area": {
      "x": 0.0,
      "y": 33.0,
      "width": 1512.0,
      "height": 949.0
    },
    "is_primary": true
  },
  "image_url": "http://wisp.localhost/capture%2F3%2F1%2Ffull",
  "image_size": {
    "width": 3024,
    "height": 1964
  },
  "windows": [
    {
      "x": 100.0,
      "y": 80.0,
      "width": 900.0,
      "height": 600.0
    },
    {
      "x": 0.0,
      "y": 0.0,
      "width": 1512.0,
      "height": 33.0
    }
  ],
  "mode": "ocr_translate",
  "window_detect": true,
  "fly_animation": true,
  "reduced_motion": false,
  "frozen_native": true,
  "half_url": null,
  "region_url": "capture/3/31/region"
} satisfies T.OverlayInit

export const OverlaySessionStart = {
  "session_id": 4,
  "monitor_id": "\\\\.\\DISPLAY1"
} satisfies T.OverlaySessionStart

export const OverlaySessionEnd = {
  "session_id": 4,
  "reason": "retrigger"
} satisfies T.OverlaySessionEnd

export const OverlayCommit = {
  "session_id": 3,
  "monitor_id": "1",
  "rect": {
    "x": 220.0,
    "y": 140.0,
    "width": 420.0,
    "height": 156.0
  },
  "kind": "drag"
} satisfies T.OverlayCommit

export const OverlayCommitAck = {
  "fly_to": {
    "x": 220.0,
    "y": 308.0,
    "width": 440.0,
    "height": 220.0
  }
} satisfies T.OverlayCommitAck

export const SettingsSnapshot = {
  "settings": {
    "schema_version": 1,
    "general": {
      "launch_at_login": true,
      "start_hidden": true,
      "ui_language": "system",
      "update_policy": "notify",
      "developer_mode": false
    },
    "hotkeys": {
      "bindings": {},
      "bobber": {
        "enabled": false
      },
      "double_tap": {
        "enabled": false,
        "key": null,
        "interval_ms": 300
      },
      "mouse": {
        "enabled": false,
        "long_press": false,
        "middle": {
          "enabled": true,
          "action": "translate_selection"
        },
        "back": {
          "enabled": false,
          "action": "clipboard"
        },
        "forward": {
          "enabled": false,
          "action": "ocr"
        }
      },
      "app_keymaps": [],
      "copy_wait_ms": 150
    },
    "providers": [],
    "defaults": {
      "translate": null,
      "ocr": null
    },
    "translate": {
      "primary_lang": "zh-CN",
      "secondary_lang": "en",
      "default_style": "natural",
      "long_text_threshold": 3000,
      "close_on_click_outside": true,
      "close_on_mouse_leave": false,
      "mouse_leave_px": 160,
      "anchor": "selection",
      "input_anchor": "cursor",
      "cache_enabled": true,
      "show_source_by_default": false
    },
    "ocr": {
      "default_mode": "lines",
      "max_edge_px": 2048,
      "auto_copy": true,
      "history": false,
      "history_limit": 50,
      "window_detect": true,
      "fly_animation": true,
      "direct_translate_transcribe": false,
      "image_detail": "high"
    },
    "clipboard": {
      "enabled": true,
      "record_text": true,
      "record_rich_text": true,
      "record_images": true,
      "record_files": true,
      "max_items": 1000,
      "max_age_days": null,
      "max_text_bytes": 1048576,
      "max_image_bytes": 20971520,
      "excluded_apps": [
        {
          "os": "macos",
          "app_id": "com.1password.1password",
          "name": "1Password"
        },
        {
          "os": "macos",
          "app_id": "com.agilebits.onepassword7",
          "name": "1Password 7"
        },
        {
          "os": "macos",
          "app_id": "com.bitwarden.desktop",
          "name": "Bitwarden"
        },
        {
          "os": "macos",
          "app_id": "org.keepassxc.keepassxc",
          "name": "KeePassXC"
        },
        {
          "os": "macos",
          "app_id": "com.apple.keychainaccess",
          "name": "Keychain Access"
        },
        {
          "os": "windows",
          "app_id": "1password.exe",
          "name": "1Password"
        },
        {
          "os": "windows",
          "app_id": "bitwarden.exe",
          "name": "Bitwarden"
        },
        {
          "os": "windows",
          "app_id": "keepassxc.exe",
          "name": "KeePassXC"
        },
        {
          "os": "windows",
          "app_id": "keepass.exe",
          "name": "KeePass"
        },
        {
          "os": "linux",
          "app_id": "1password",
          "name": "1Password"
        },
        {
          "os": "linux",
          "app_id": "bitwarden",
          "name": "Bitwarden"
        },
        {
          "os": "linux",
          "app_id": "keepassxc",
          "name": "KeePassXC"
        }
      ],
      "secret_detection": true,
      "secret_tray_flash": true,
      "panel_position": "center",
      "paste_on": "single_click",
      "show_hint_bar": true
    },
    "appearance": {
      "theme": "system",
      "accent": "system",
      "font_size": 14,
      "translate_min_width": 360,
      "translate_max_width": 520,
      "clipboard_width": 420,
      "max_height_ratio": 0.6,
      "material": "system",
      "reduce_motion": "system"
    },
    "advanced": {
      "log_level": "info",
      "webview_low_memory_when_hidden": true,
      "unload_panel_after_idle_min": null
    }
  },
  "styles": [
    {
      "id": "natural",
      "name": "",
      "builtin": true,
      "system_template": "You are a professional translator. Translate {{source_lang}} into {{target_lang}}.",
      "tail_instruction": "Reply with the translation only.",
      "temperature": 0.3,
      "output": "plain_text",
      "dictionary_mode": true,
      "model_ref": null,
      "hotkey": null,
      "modified": false
    },
    {
      "id": "custom-3f9a1c2e",
      "name": "Release notes",
      "builtin": false,
      "system_template": "Translate release notes from {{source_lang}} into {{target_lang}}.",
      "tail_instruction": "Reply with the translation only.",
      "temperature": null,
      "output": "markdown_lite",
      "dictionary_mode": false,
      "model_ref": {
        "provider_id": "openai-main",
        "model_id": "gpt-5-mini"
      },
      "hotkey": "Control+Alt+KeyR",
      "modified": false
    }
  ],
  "runtime": {
    "version": "0.1.0",
    "platform": {
      "os": "macos",
      "os_version": "26.4",
      "arch": "aarch64",
      "session": "native",
      "capabilities": {
        "global_hotkeys": {
          "available": true,
          "reason_key": null
        },
        "selection": {
          "available": true,
          "reason_key": null
        },
        "simulated_copy": {
          "available": true,
          "reason_key": null
        },
        "paste_inject": {
          "available": true,
          "reason_key": null
        },
        "capture_overlay": {
          "available": true,
          "reason_key": null
        },
        "clipboard_watch": {
          "available": true,
          "reason_key": null
        },
        "input_hooks": {
          "available": false,
          "reason_key": "errors.platform.input_monitoring_missing"
        }
      }
    },
    "has_key": {
      "openai-main": true
    },
    "keychain": "available",
    "hotkeys": [
      {
        "target": {
          "kind": "action",
          "action": "translate_selection"
        },
        "binding": "default",
        "effective": "Alt+KeyD",
        "display": "⌥D",
        "state": "registered",
        "reason_key": null
      }
    ],
    "ext_triggers": {
      "bobber": "off",
      "double_tap": "waiting_permission",
      "mouse": "off",
      "reason_key": "hotkeys.ext.need_input_monitoring"
    },
    "permissions": {
      "accessibility": "granted",
      "screen_recording": "not_determined",
      "input_monitoring": "denied",
      "lost_after_update": false
    },
    "clipboard_paused": false,
    "update": {
      "state": "idle",
      "version": null,
      "notes": null,
      "progress": null,
      "error": null
    },
    "glossary_count": 0,
    "settings_issue": {
      "kind": "reset_to_default",
      "file": "settings.json",
      "backup_path": "/Users/me/Library/Application Support/wisp/settings.json.bak-1760000000",
      "at_ms": 1760000000000
    },
    "orphan_keys": 1
  }
} satisfies T.SettingsSnapshot

export const RuntimeInfo = {
  "version": "0.1.0",
  "platform": {
    "os": "macos",
    "os_version": "26.4",
    "arch": "aarch64",
    "session": "native",
    "capabilities": {
      "global_hotkeys": {
        "available": true,
        "reason_key": null
      },
      "selection": {
        "available": true,
        "reason_key": null
      },
      "simulated_copy": {
        "available": true,
        "reason_key": null
      },
      "paste_inject": {
        "available": true,
        "reason_key": null
      },
      "capture_overlay": {
        "available": true,
        "reason_key": null
      },
      "clipboard_watch": {
        "available": true,
        "reason_key": null
      },
      "input_hooks": {
        "available": false,
        "reason_key": "errors.platform.input_monitoring_missing"
      }
    }
  },
  "has_key": {
    "openai-main": true
  },
  "keychain": "available",
  "hotkeys": [
    {
      "target": {
        "kind": "action",
        "action": "translate_selection"
      },
      "binding": "default",
      "effective": "Alt+KeyD",
      "display": "⌥D",
      "state": "registered",
      "reason_key": null
    }
  ],
  "ext_triggers": {
    "bobber": "off",
    "double_tap": "waiting_permission",
    "mouse": "off",
    "reason_key": "hotkeys.ext.need_input_monitoring"
  },
  "permissions": {
    "accessibility": "granted",
    "screen_recording": "not_determined",
    "input_monitoring": "denied",
    "lost_after_update": false
  },
  "clipboard_paused": false,
  "update": {
    "state": "idle",
    "version": null,
    "notes": null,
    "progress": null,
    "error": null
  },
  "glossary_count": 0,
  "settings_issue": {
    "kind": "reset_to_default",
    "file": "settings.json",
    "backup_path": "/Users/me/Library/Application Support/wisp/settings.json.bak-1760000000",
    "at_ms": 1760000000000
  },
  "orphan_keys": 1
} satisfies T.RuntimeInfo

export const SettingsFileIssue = {
  "kind": "invalid_json",
  "file": "styles.json",
  "backup_path": null,
  "at_ms": 1760000123456
} satisfies T.SettingsFileIssue

export const SettingsFileIssue__newer_version = {
  "kind": "newer_version",
  "file": "settings.json",
  "backup_path": null,
  "at_ms": 1760000234567
} satisfies T.SettingsFileIssue

export const Boot = {
  "label": "settings",
  "os": "windows",
  "os_version": "10.0.26100",
  "theme": "dark",
  "accent": "#0A84FF",
  "reduce_motion": false,
  "reduce_transparency": false,
  "material": "native",
  "frame_mode": "css",
  "shadow_margin": 24.0,
  "font_size": 14,
  "ui_language": "zh-CN",
  "dev": true,
  "debug_build": true,
  "route": {
    "kind": "settings",
    "section": "providers"
  }
} satisfies T.Boot

export const Boot__panel_cold = {
  "label": "panel",
  "os": "macos",
  "os_version": "26.4",
  "theme": "light",
  "accent": "",
  "reduce_motion": false,
  "reduce_transparency": false,
  "material": "native",
  "frame_mode": "native",
  "shadow_margin": 0.0,
  "font_size": 14,
  "ui_language": "en-US",
  "dev": false,
  "debug_build": false,
  "route": null,
  "prepare": {
    "session_id": 12,
    "view": "input",
    "payload": {
      "kind": "input",
      "initial_text": "",
      "target_lang": null,
      "hint": {
        "reason": "no_provider"
      }
    },
    "width": 420.0,
    "max_height": 569.4,
    "placement": "center",
    "origin_x": 210.0,
    "focused": true,
    "transition": "enter"
  }
} satisfies T.Boot

export const AppearanceState = {
  "theme": "dark",
  "accent": "#3478F6",
  "reduce_motion": false,
  "reduce_transparency": false,
  "material": "native",
  "font_size": 14,
  "os": "macos",
  "ui_language": "zh-CN"
} satisfies T.AppearanceState

export const AppMeta = {
  "version": "0.1.0",
  "platform": {
    "os": "macos",
    "os_version": "26.4",
    "arch": "aarch64",
    "session": "native",
    "capabilities": {
      "global_hotkeys": {
        "available": true,
        "reason_key": null
      },
      "selection": {
        "available": true,
        "reason_key": null
      },
      "simulated_copy": {
        "available": true,
        "reason_key": null
      },
      "paste_inject": {
        "available": true,
        "reason_key": null
      },
      "capture_overlay": {
        "available": true,
        "reason_key": null
      },
      "clipboard_watch": {
        "available": true,
        "reason_key": null
      },
      "input_hooks": {
        "available": false,
        "reason_key": "errors.platform.input_monitoring_missing"
      }
    }
  },
  "config_dir": "/Users/me/Library/Application Support/wisp",
  "data_dir": "/Users/me/Library/Application Support/wisp",
  "log_dir": "/Users/me/Library/Logs/wisp",
  "dev_mode": false
} satisfies T.AppMeta

export const UpdateStatus = {
  "state": "downloading",
  "version": "0.1.1",
  "notes": "Bug fixes.",
  "progress": 0.5,
  "error": null
} satisfies T.UpdateStatus

export const OnboardingHotkey = {
  "action": "ocr"
} satisfies T.EventMap['onboarding:hotkey']

export const HotkeyTestReceived = {
  "target": {
    "kind": "style",
    "style_id": "casual"
  }
} satisfies T.EventMap['hotkey:test_received']

export const Settings = {
  "schema_version": 1,
  "general": {
    "launch_at_login": true,
    "start_hidden": true,
    "ui_language": "system",
    "update_policy": "notify",
    "developer_mode": false
  },
  "hotkeys": {
    "bindings": {},
    "bobber": {
      "enabled": false
    },
    "double_tap": {
      "enabled": false,
      "key": null,
      "interval_ms": 300
    },
    "mouse": {
      "enabled": false,
      "long_press": false,
      "middle": {
        "enabled": true,
        "action": "translate_selection"
      },
      "back": {
        "enabled": false,
        "action": "clipboard"
      },
      "forward": {
        "enabled": false,
        "action": "ocr"
      }
    },
    "app_keymaps": [],
    "copy_wait_ms": 150
  },
  "providers": [],
  "defaults": {
    "translate": null,
    "ocr": null
  },
  "translate": {
    "primary_lang": "zh-CN",
    "secondary_lang": "en",
    "default_style": "natural",
    "long_text_threshold": 3000,
    "close_on_click_outside": true,
    "close_on_mouse_leave": false,
    "mouse_leave_px": 160,
    "anchor": "selection",
    "input_anchor": "cursor",
    "cache_enabled": true,
    "show_source_by_default": false
  },
  "ocr": {
    "default_mode": "lines",
    "max_edge_px": 2048,
    "auto_copy": true,
    "history": false,
    "history_limit": 50,
    "window_detect": true,
    "fly_animation": true,
    "direct_translate_transcribe": false,
    "image_detail": "high"
  },
  "clipboard": {
    "enabled": true,
    "record_text": true,
    "record_rich_text": true,
    "record_images": true,
    "record_files": true,
    "max_items": 1000,
    "max_age_days": null,
    "max_text_bytes": 1048576,
    "max_image_bytes": 20971520,
    "excluded_apps": [
      {
        "os": "macos",
        "app_id": "com.1password.1password",
        "name": "1Password"
      },
      {
        "os": "macos",
        "app_id": "com.agilebits.onepassword7",
        "name": "1Password 7"
      },
      {
        "os": "macos",
        "app_id": "com.bitwarden.desktop",
        "name": "Bitwarden"
      },
      {
        "os": "macos",
        "app_id": "org.keepassxc.keepassxc",
        "name": "KeePassXC"
      },
      {
        "os": "macos",
        "app_id": "com.apple.keychainaccess",
        "name": "Keychain Access"
      },
      {
        "os": "windows",
        "app_id": "1password.exe",
        "name": "1Password"
      },
      {
        "os": "windows",
        "app_id": "bitwarden.exe",
        "name": "Bitwarden"
      },
      {
        "os": "windows",
        "app_id": "keepassxc.exe",
        "name": "KeePassXC"
      },
      {
        "os": "windows",
        "app_id": "keepass.exe",
        "name": "KeePass"
      },
      {
        "os": "linux",
        "app_id": "1password",
        "name": "1Password"
      },
      {
        "os": "linux",
        "app_id": "bitwarden",
        "name": "Bitwarden"
      },
      {
        "os": "linux",
        "app_id": "keepassxc",
        "name": "KeePassXC"
      }
    ],
    "secret_detection": true,
    "secret_tray_flash": true,
    "panel_position": "center",
    "paste_on": "single_click",
    "show_hint_bar": true
  },
  "appearance": {
    "theme": "system",
    "accent": "system",
    "font_size": 14,
    "translate_min_width": 360,
    "translate_max_width": 520,
    "clipboard_width": 420,
    "max_height_ratio": 0.6,
    "material": "system",
    "reduce_motion": "system"
  },
  "advanced": {
    "log_level": "info",
    "webview_low_memory_when_hidden": true,
    "unload_panel_after_idle_min": null
  }
} satisfies T.Settings

export const General = {
  "launch_at_login": false,
  "start_hidden": true,
  "ui_language": "en-US",
  "update_policy": "auto_install",
  "developer_mode": true
} satisfies T.Settings['general']

export const HotkeySettings = {
  "bindings": {
    "translate_selection": "Alt+KeyD",
    "clipboard": "none"
  },
  "bobber": {
    "enabled": true
  },
  "double_tap": {
    "enabled": true,
    "key": "right_alt",
    "interval_ms": 300
  },
  "mouse": {
    "enabled": false,
    "long_press": false,
    "middle": {
      "enabled": true,
      "action": "translate_selection"
    },
    "back": {
      "enabled": false,
      "action": "clipboard"
    },
    "forward": {
      "enabled": false,
      "action": "ocr"
    }
  },
  "app_keymaps": [
    {
      "os": "linux",
      "app_id": "gnome-terminal-server",
      "copy": "Control+Shift+KeyC",
      "paste": "Control+Shift+KeyV"
    }
  ],
  "copy_wait_ms": 150
} satisfies T.Settings['hotkeys']

export const BobberSettings = {
  "enabled": true
} satisfies T.Settings['hotkeys']['bobber']

export const DoubleTapSettings = {
  "enabled": true,
  "key": "right_ctrl",
  "interval_ms": 200
} satisfies T.Settings['hotkeys']['double_tap']

export const MouseSettings = {
  "enabled": true,
  "long_press": true,
  "middle": {
    "enabled": true,
    "action": "translate_selection"
  },
  "back": {
    "enabled": false,
    "action": "clipboard"
  },
  "forward": {
    "enabled": false,
    "action": "ocr"
  }
} satisfies T.Settings['hotkeys']['mouse']

export const MouseBinding = {
  "enabled": true,
  "action": "translate_selection"
} satisfies T.Settings['hotkeys']['mouse']['middle']

export const AppKeymap = {
  "os": "linux",
  "app_id": "gnome-terminal-server",
  "copy": "Control+Shift+KeyC",
  "paste": "Control+Shift+KeyV"
} satisfies T.Settings['hotkeys']['app_keymaps'][number]

export const TranslateSettings = {
  "primary_lang": "en",
  "secondary_lang": "ja",
  "default_style": "technical",
  "long_text_threshold": 3000,
  "close_on_click_outside": true,
  "close_on_mouse_leave": false,
  "mouse_leave_px": 160,
  "anchor": "cursor",
  "input_anchor": "center",
  "cache_enabled": true,
  "show_source_by_default": false
} satisfies T.Settings['translate']

export const OcrSettings = {
  "default_mode": "paragraphs",
  "max_edge_px": 2048,
  "auto_copy": true,
  "history": true,
  "history_limit": 50,
  "window_detect": true,
  "fly_animation": true,
  "direct_translate_transcribe": false,
  "image_detail": "auto"
} satisfies T.Settings['ocr']

export const ClipboardSettings = {
  "enabled": true,
  "record_text": true,
  "record_rich_text": true,
  "record_images": true,
  "record_files": true,
  "max_items": 1000,
  "max_age_days": 30,
  "max_text_bytes": 1048576,
  "max_image_bytes": 20971520,
  "excluded_apps": [
    {
      "os": "macos",
      "app_id": "com.bitwarden.desktop",
      "name": "Bitwarden"
    }
  ],
  "secret_detection": true,
  "secret_tray_flash": true,
  "panel_position": "remember",
  "paste_on": "double_click",
  "show_hint_bar": true
} satisfies T.Settings['clipboard']

export const ExcludedApp = {
  "os": "macos",
  "app_id": "com.bitwarden.desktop",
  "name": "Bitwarden"
} satisfies T.Settings['clipboard']['excluded_apps'][number]

export const Appearance = {
  "theme": "dark",
  "accent": "#FF6B35",
  "font_size": 14,
  "translate_min_width": 360,
  "translate_max_width": 520,
  "clipboard_width": 420,
  "max_height_ratio": 0.75,
  "material": "solid",
  "reduce_motion": "on"
} satisfies T.Settings['appearance']

export const Advanced = {
  "log_level": "debug",
  "webview_low_memory_when_hidden": false,
  "unload_panel_after_idle_min": 60
} satisfies T.Settings['advanced']
