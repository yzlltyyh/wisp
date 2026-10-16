# 模型服务

Wisp 不内置模型或 Key：翻译与 OCR 通过你自己接入的服务完成，也可以使用下载到本机运行的离线模型（见[使用说明 · 离线模型](user-guide.md#离线模型)）。本文说明 Wisp 支持哪些协议、Base URL 怎么填、模型与识图能力如何判定、PaddleOCR 文字识别服务怎么接入，以及各家服务需要注意的地方。日常操作见 [README · 接入模型服务](../README.md#接入模型服务) 与[使用说明](user-guide.md#模型与服务)；实现位于 `crates/wisp-llm`，PaddleOCR 的客户端在 `crates/wisp-local`（见[架构](architecture.md#rust-crate)）。

## 目录

- [四种协议](#四种协议)
- [Base URL](#base-url)
- [服务模板](#服务模板)
- [模型与识图能力](#模型与识图能力)
- [「思考」与请求参数](#思考与请求参数)
- [连接测试、超时与重试](#连接测试超时与重试)
- [错误提示](#错误提示)
- [各服务的注意事项](#各服务的注意事项)
- [服务编辑器的高级选项](#服务编辑器的高级选项)
- [PaddleOCR（文字识别服务）](#paddleocr文字识别服务)
- [多设备同步](#多设备同步)
- [贡献者：代码在哪里](#贡献者代码在哪里)

## 四种协议

| 协议 | 请求 | 鉴权 | 说明 |
| --- | --- | --- | --- |
| OpenAI Chat Completions | `POST {base}/chat/completions` | `Authorization: Bearer` | 兼容面最广：绝大多数服务、中转站与本机模型都支持 |
| OpenAI Responses | `POST {base}/responses` | `Authorization: Bearer` | OpenAI 的新接口；Wisp 一律发送 `store: false`，不在服务端留存 |
| Anthropic Messages | `POST {base}/messages` | `x-api-key` + `anthropic-version` | Claude 系列 |
| Google Gemini | `POST {base}/models/{模型}:streamGenerateContent?alt=sse` | `x-goog-api-key` | Gemini 系列，对话模型都能识图 |

四种协议都使用流式输出（SSE）。Wisp 把它们统一为同一种事件流：开始、增量文本、结束（含用量）或错误。推理模型输出的「思考」内容（`reasoning_content`、thinking block、`thought: true` 的 part 等）一律不显示。

服务编辑器的协议列表里另有一项「PaddleOCR」：它不是对话模型的协议，而是只用于 OCR 的文字识别服务，地址、鉴权与结果格式都不同，见[下文](#paddleocr文字识别服务)。本节和下一节的规则对它不适用。

## Base URL

填写到版本段为止即可，不要带 `/chat/completions` 之类的接口路径。编辑时，输入框下方实时显示最终的请求地址。

规范化规则：

1. 去掉首尾空白和末尾的 `/`；没有 `http(s)://` 时补上（`localhost`、`127.0.0.1` 等回环地址补 `http://`，其他补 `https://`）。
2. 地址以 `#` 结尾时，表示按填写的路径原样请求：去掉 `#`，不再追加版本段。
3. 路径里已经有版本段（匹配 `/v数字`，可带 `alpha` / `beta`，例如 `/v1`、`/v3`、`/v1beta`）时原样使用。
4. 否则追加版本段：OpenAI 两种协议与 Anthropic 追加 `/v1`，Gemini 追加 `/v1beta`。
5. 之后各协议只拼固定的资源名（见上表）。

粘贴进来的地址常常带着多余的部分，Wisp 会自动处理并在预览下方提示：

- 末尾带着接口路径（`…/v1/chat/completions`、`…/messages`、`…/models/<模型>:streamGenerateContent`）时去掉它；
- 带查询串（例如 Azure 风格的 `?api-version=…`）时，查询串会附加到每个请求的末尾；
- 地址中间的 `#…` 片段会被丢弃。

| 填写 | 协议 | 最终请求 |
| --- | --- | --- |
| `https://api.openai.com` | OpenAI Chat | `https://api.openai.com/v1/chat/completions` |
| `https://api.openai.com/v1` | OpenAI Responses | `https://api.openai.com/v1/responses` |
| `https://api.deepseek.com` | OpenAI Chat | `https://api.deepseek.com/v1/chat/completions` |
| `https://dashscope.aliyuncs.com/compatible-mode/v1` | OpenAI Chat | `…/compatible-mode/v1/chat/completions` |
| `https://open.bigmodel.cn/api/paas/v4` | OpenAI Chat | `…/api/paas/v4/chat/completions` |
| `https://ark.cn-beijing.volces.com/api/v3` | OpenAI Chat | `…/api/v3/chat/completions` |
| `https://api.anthropic.com` | Anthropic | `https://api.anthropic.com/v1/messages` |
| `https://api.deepseek.com/anthropic` | Anthropic | `https://api.deepseek.com/anthropic/v1/messages` |
| `https://generativelanguage.googleapis.com` | Gemini | `…/v1beta/models/{模型}:streamGenerateContent?alt=sse` |
| `http://localhost:11434` | OpenAI Chat | `http://localhost:11434/v1/chat/completions` |
| `https://relay.example.com/openai#` | OpenAI Chat | `https://relay.example.com/openai/chat/completions` |

连接测试返回 404 时，多半是版本段不对：对照预览确认 `/v1` 是否多了或少了；服务的路径本来就不带版本段时，在末尾加 `#`。

**中转站**（new-api、one-api 等聚合服务）通常同时提供几种协议的原生路径：填站点根地址，按需要选协议——OpenAI 两种协议走 `{站点}/v1/…`，Anthropic 走 `{站点}/v1/messages`，Gemini 走 `{站点}/v1beta/models/…`。

## 服务模板

「添加服务」时选择模板，模板预填协议、Base URL 与推荐参数，全部可以修改。

| 模板 | 协议 | 默认 Base URL | 地区 / 变体 |
| --- | --- | --- | --- |
| OpenAI | OpenAI Chat | `https://api.openai.com/v1` | — |
| OpenAI（Responses API） | OpenAI Responses | `https://api.openai.com/v1` | — |
| Anthropic | Anthropic | `https://api.anthropic.com` | — |
| Google Gemini | Gemini | `https://generativelanguage.googleapis.com` | — |
| DeepSeek | OpenAI Chat | `https://api.deepseek.com` | — |
| OpenRouter | OpenAI Chat | `https://openrouter.ai/api/v1` | — |
| 通义千问（阿里云百炼） | OpenAI Chat | `https://dashscope.aliyuncs.com/compatible-mode/v1` | 中国大陆、国际、业务空间 |
| 智谱 GLM | OpenAI Chat | `https://open.bigmodel.cn/api/paas/v4` | — |
| 火山方舟（豆包） | OpenAI Chat | `https://ark.cn-beijing.volces.com/api/v3` | — |
| 月之暗面 Kimi | OpenAI Chat | `https://api.moonshot.cn/v1` | 中国大陆、国际（`api.moonshot.ai`） |
| 硅基流动 | OpenAI Chat | `https://api.siliconflow.cn/v1` | — |
| MiniMax | OpenAI Chat | `https://api.minimaxi.com/v1` | 中国大陆、国际（`api.minimax.io`） |
| Groq | OpenAI Chat | `https://api.groq.com/openai/v1` | — |
| Ollama（本机） | OpenAI Chat | `http://localhost:11434` | — |
| LM Studio（本机） | OpenAI Chat | `http://localhost:1234` | — |
| 中转站 / 自定义兼容端点 | 任选 | 自行填写 | — |
| PaddleOCR（文字识别，单独一组） | PaddleOCR | `https://paddleocr.aistudio-app.com/api/v2/ocr/jobs` | 也可以填部署的服务或自托管的 PaddleX，见[下文](#paddleocr文字识别服务) |

本机模板（Ollama、LM Studio）不要求 Key，首字超时与空闲超时放宽到 120 秒，以容纳模型的加载时间。它们连接的是你自己运行的模型服务；Wisp 自己下载并在本机运行的是「离线模型」，不需要添加服务，在「设置 → 离线模型」里管理。

服务列表与模板里显示的服务商标志是各自所有者的商标，只用来标明对应的服务。

## 模型与识图能力

**模型列表**：连接测试与「拉取模型」从服务的模型列表端点获取（OpenAI 兼容端点 `GET /models`，Anthropic `GET /models?limit=1000`，Gemini `GET /models?pageSize=1000`，最多翻 5 页），与已保存的模型合并。Gemini 只保留支持 `generateContent` 的对话模型；Anthropic 过滤已退役的模型。拉取失败时可以手动添加模型 ID。

**识图能力**是三态：支持、不支持、未确认。按以下优先级判定，高的覆盖低的：

1. **手动设置**：在模型表里把「图片」列设为支持或不支持；
2. **运行时反馈**：用某个模型识图时，服务明确回答「不支持图片输入」，就标为不支持（只针对「未确认」的模型，不覆盖手动设置）；单张图片处理失败（损坏、解码失败）不算；
3. **能力字段**：Anthropic 模型列表的 `capabilities.image_input`、OpenRouter 的 `architecture.input_modalities`、Gemini 的对话模型（全部支持）；
4. **模型名关键词**：常见的视觉模型系列（如 `gpt-4o`、`gpt-4.1`、`gpt-5`、`claude-*`、`gemini-*`、`qwen*-vl`、`glm-4v`、`llava`、`pixtral` 等）判为支持；语音、嵌入、图像生成、代码专用等模型判为不支持；其余为未确认。

OCR 的模型选择器只列「支持」与「未确认」的模型，另外列出离线模型与 PaddleOCR 服务的模型。PaddleOCR 服务的模型按协议判定：可以用于 OCR、不能用于翻译，不看「图片」列。中转站返回的模型 ID 常常是别名，关键词判定只能作为初值，请按实际情况手动改。「重置能力缓存」让 Wisp 下一次请求时重新探测。

**推理模型**：模型表的「推理」列标出推理模型（能力字段、手动设置，或 ID 匹配 `o1`–`o9`、`gpt-5`、`gpt-6` 开头）。推理模型不发送 `temperature`，最大输出至少为 1024，并可以在模型的高级设置里选择推理档位。

## 「思考」与请求参数

翻译不需要模型先「思考」，思考只会显著拉长首字延迟。服务编辑器高级设置里的「关闭思考」决定用哪种方式关掉或调低它，模板已经预填：

| 方式 | 发送的参数 | 预填于 |
| --- | --- | --- |
| OpenAI effort | 推理模型：`reasoning_effort: "low"`（Responses：`reasoning.effort`），可在模型设置里改为 `minimal` / `none` | OpenAI、OpenAI Responses |
| Anthropic effort | `output_config.effort`：翻译 `low`、OCR `medium`；只有模型列表声明支持关闭思考时才发 `thinking: {type: "disabled"}` | Anthropic |
| Gemini level | Gemini 3 及之后：`thinkingConfig.thinkingLevel: "LOW"`；Gemini 2.5 Flash（含 Lite）：`thinkingBudget: 0`；其余不发 | Google Gemini |
| `thinking: {type: "disabled"}` | 同左 | DeepSeek、智谱 GLM、火山方舟、MiniMax |
| `enable_thinking: false` | 同左 | 通义千问、硅基流动 |
| Kimi | `reasoning_effort: "low"` | 月之暗面 Kimi |
| 不处理 | — | OpenRouter、Groq、Ollama、LM Studio、中转站 |

其他参数：

- **temperature**：取自翻译风格；Anthropic 协议一律不发（新模型只接受默认值），推理模型不发。
- **最大输出**：OpenAI Chat 协议有 `max_completion_tokens` 与 `max_tokens` 两种字段名，模板按服务预填；Responses 用 `max_output_tokens`，Anthropic 用必填的 `max_tokens`，Gemini 用 `generationConfig.maxOutputTokens`。
- **自动调整**：服务以 400 拒绝某个参数（例如不支持 `temperature`、要求另一种最大输出字段、不认识思考参数）时，Wisp 去掉或换掉这个参数自动重试一次，并把结论记进这个模型的能力缓存，之后的请求直接按新规则发送。
- **图片**：截图按长边缩放（默认 2048 px，可在「设置 → OCR」调整）后以内联 base64 发送；OpenAI 两种协议的 `detail` 按「图片细节」设置，「原图」只对 OpenAI 官方端点生效，其他端点退回「高」。
- **流式用量**：OpenAI Chat 协议发送 `stream_options.include_usage`；对 OpenAI 官方端点另发 `include_obfuscation: false`，减少流量。

## 连接测试、超时与重试

**连接测试**同时做两件事：拉取模型列表，并用第一个已启用的对话模型发一条最大输出为 1 的流式请求。两项都成功为「连接正常」；模型列表失败而流式请求成功为「部分可用」（可以手动添加模型 ID 后使用）；其余为失败，错误卡片里可以展开服务返回的原始信息。

**超时**（每个服务可单独调整）：

| 项 | 默认 | 本机模板 |
| --- | --- | --- |
| 连接 | 10 s | 10 s |
| 首字（从收到响应头起算） | 30 s | 120 s |
| 流中空闲 | 60 s | 120 s |

**重试**：只在收到成功的响应头之前重试，最多一次传输重试（连接失败、5xx 等，退避约 500 ms，服务给出不超过 5 s 的 `Retry-After` 时按它等待）加一次参数调整重试；429 限流不自动重试；流开始之后不重试，断流时保留已收到的文字。

**代理**：Wisp 使用系统代理；本机地址直连。在 macOS 与 Windows 上切换代理后无需重启。

## 错误提示

服务返回的错误被归为以下几类，面板里显示对应的说明与操作按钮：

| 类别 | 常见原因 |
| --- | --- |
| Key 无效或无权限 | 401 / 403；Gemini 的 Key 无效时返回 400 `API_KEY_INVALID`，同样归为这一类 |
| 模型或地址不存在 | 404：模型 ID 写错，或 Base URL 的路径不对（对照预览里的最终地址） |
| 模型不支持图片 / 图片过大 | 用不支持识图的模型做 OCR；图片超过服务的上限 |
| 文本过长 | 超出模型的上下文长度 |
| 额度用尽 | 429 且为额度或计费原因、Anthropic 402 等；不会自动重试 |
| 触发限流 | 其他 429，稍后重试 |
| 服务端错误 | 5xx、过载 |
| 内容被拦截 | 服务的安全策略拒绝了这段内容（Gemini 的 `SAFETY` / `RECITATION` 等），可以换一个模型 |
| 网络错误 / 超时 | 检查网络与代理 |
| 无法解析的响应 | 返回的不是预期的格式，常见于 Base URL 指向了网页而不是 API |
| 输出被截断 | 达到最大输出；推理模型的思考可能耗尽输出额度，可以在错误卡片里调高上限或换模型 |

错误信息里回显的 Key 会被替换为 `***`；日志只记录 host 与路径，不记录 Key、请求头的值与查询参数。

## 各服务的注意事项

- **OpenAI**：推理模型（`o` 系列、`gpt-5` 及之后）只接受默认的 `temperature`，新模型使用 `max_completion_tokens`，这些 Wisp 都会自动处理。需要「原图」细节识别时使用官方端点。
- **Anthropic**：Claude 新模型默认开启思考、不接受自定义 `temperature`；Wisp 用 `output_config.effort` 控制速度。经中转站访问时，Wisp 对 `api.anthropic.com` 以外的地址同时发送 `x-api-key` 与 `Authorization: Bearer` 两种鉴权头。
- **Google Gemini**：Key 无效时返回 400 而不是 401。Wisp 使用 `streamGenerateContent`；如果 Google 将来下线这组接口，可以改用 Gemini 的 OpenAI 兼容端点：协议选 OpenAI Chat Completions，Base URL 填 `https://generativelanguage.googleapis.com/v1beta/openai`。部分代理只接受 URL 参数形式的 Key，可以打开高级选项「Key 放在 URL 参数中」。
- **DeepSeek**：思考默认开启，模板预填关闭方式。官方地址不带 `/v1`，Wisp 会自动补上，两种写法服务端都接受。DeepSeek 另有 Anthropic 兼容端点 `https://api.deepseek.com/anthropic`。
- **OpenRouter**：模型列表带输入模态信息，识图能力判定最准确。流中错误以 200 状态加错误块的形式出现，Wisp 会正确识别为失败。
- **通义千问（阿里云百炼）**：Qwen 3.5 起思考默认开启，模板预填 `enable_thinking: false`。视觉模型官方建议不要设置系统提示，模板默认打开「合并系统提示」（只对带图的请求生效）。国际站与业务空间的地址在「区域」里切换。
- **智谱 GLM**：路径版本段是 `/api/paas/v4`，原样使用即可。
- **火山方舟（豆包）**：路径版本段是 `/api/v3`；模型 ID 常带日期后缀，也可以填推理接入点 ID（`ep-…`）。
- **月之暗面 Kimi**：新模型默认的推理强度很高，模板预填 `reasoning_effort: "low"`。国际站在「区域」里切换。
- **硅基流动**：思考开关同通义（`enable_thinking`）；错误响应不是 OpenAI 的结构，Wisp 按状态码与错误码归类。
- **MiniMax**：部分模型把思考内容以 `<think>` 标签内联在正文开头，Wisp 会剥离；成功状态码下也可能返回错误体（`base_resp`），同样会被识别。国际站在「区域」里切换。
- **Groq**：标准的 OpenAI 兼容端点，只有视觉模型接受图片。
- **Ollama**：填 `http://localhost:11434`，Wisp 自动补 `/v1`；不需要 Key。上下文长度由模型配置（`num_ctx`）决定，长文本翻译时请留意。
- **LM Studio**：填 `http://localhost:1234`；带视觉投影（mmproj）的模型才能识图。推理模型可能把 `<think>` 内联在正文里，「剥离 `<think>` 标签」默认打开。
- **中转站**：模型 ID 往往是别名，识图能力请手动确认；有的中转站会改写参数或只实现部分接口，连接测试会直接验证流式路径。

## 服务编辑器的高级选项

| 选项 | 作用 |
| --- | --- |
| 自定义请求头 | 附加到每个请求；不要在这里填 Key：请求头以明文写在 `settings.json` 里，Key 请填在「API Key」一栏，只保存在系统凭据存储里 |
| 关闭思考 | 见[上文](#思考与请求参数) |
| 输出长度参数 | OpenAI Chat 协议使用的最大输出字段名；被拒绝时 Wisp 会自动换用另一个 |
| 合并系统提示 | 带图请求不发系统提示，把它合并进用户消息（部分视觉模型需要） |
| 剥离 `<think>` 标签 | 只对 OpenAI Chat 协议生效；只剥离出现在输出开头的思考块，正文中间的同名标签原样保留 |
| 同时发送两种鉴权头 | Anthropic 协议同时发 `x-api-key` 与 `Authorization: Bearer`（默认：官方端点只发前者，其他地址两种都发） |
| Key 放在 URL 参数中 | Gemini 协议以 `?key=` 传 Key，而不是请求头 |
| 超时 | 连接、首字、流中空闲三项 |

## PaddleOCR（文字识别服务）

PaddleOCR 的文字识别服务作为一种模型服务添加：在「添加服务」的模板里选「PaddleOCR」。它只用于 OCR：出现在 OCR 模型的下拉与「重新识别 ▾」里，不出现在翻译的下拉里；截图直接翻译时先由它识别，再交给翻译模型。操作步骤见[使用说明 · PaddleOCR 云端识别](user-guide.md#paddleocr-云端识别)。

### 服务地址

填完整的接口地址，Wisp 原样使用（不补版本段，丢弃查询串与 `#` 片段），按路径的结尾判断接口形态，编辑器在地址下方显示判断结果：

| 地址 | 接口 | 鉴权头 | 请求方式 |
| --- | --- | --- | --- |
| `https://paddleocr.aistudio-app.com/api/v2/ocr/jobs`（默认） | AI Studio 的任务接口 | `Authorization: bearer <令牌>` | 提交 `multipart/form-data`（`file`、`model`、`optionalPayload`），轮询任务状态，完成后下载结果 |
| AI Studio 上部署的服务，以 `/ocr` 或 `/layout-parsing` 结尾 | 部署的服务 | `Authorization: token <令牌>` | `POST` JSON（`file` 为 base64，`fileType: 1`），直接返回结果 |
| 自托管 PaddleX 服务的 `/ocr`（文字识别）或 `/layout-parsing`（文档解析） | 部署的服务 | 同上；令牌留空时不发 | 同上 |

- 其他结尾的地址提示「无法识别的服务地址」，不发请求。
- `http://` 只允许本机与局域网地址（回环、私有网段、运营商级 NAT 的 `100.64.0.0/10`——Tailscale 等组网工具分配的就是这一段、链路本地、`localhost`、`.local`）；公网地址必须用 `https://`，否则令牌与截图会明文经过网络。这些地址直接连接，不经过系统代理。
- 截图以 PNG 上传（过大时改用 JPEG），上限 50 MB。Wisp 在可选参数里关闭文档方向分类、文档矫正与可视化结果；文字识别模型另关文本行方向分类，文档解析模型另关图表与印章识别。

### 访问令牌

- AI Studio 的访问令牌在 `https://aistudio.baidu.com/account/accessToken` 获取，填在编辑器的「访问令牌」一栏。它与 API Key 走同一套机制：只写入系统凭据存储，只进不出，界面不回显，日志、错误信息与诊断包里都没有。
- AI Studio 的访问令牌默认拥有账号的全部权限，不只用于 OCR，泄露的影响较大：只在自己的设备上使用，不要分享。
- 开启同步并勾选「API Key」时，令牌像 Key 一样加密后同步到你的其他设备，并同样绑定设置它时服务地址的协议、主机与端口（见[多设备同步](#多设备同步)）。

### 模型与识别模式

预置 `PP-OCRv6`、`PP-StructureV3`、`PaddleOCR-VL-1.6`，可以在模型表里手动添加服务支持的其他模型名（例如 `PP-OCRv5`、`PaddleOCR-VL-1.5`）；这个服务没有「获取模型列表」。名称以 `PP-Structure` 或 `PaddleOCR-VL` 开头的按文档解析模型处理，其余按文字识别模型处理。

| 模型 | 识别模式 | 结果 |
| --- | --- | --- |
| 文字识别模型（PP-OCRv6 等） | 纯文本、段落；不支持结构化（界面置灰，默认模式是结构化时按段落识别） | 服务返回的文本行与四点框，用与离线 PP-OCRv6 相同的规则拼成行与段落 |
| 文档解析模型（PP-StructureV3、PaddleOCR-VL 系列） | 三种都支持 | 结构化：服务返回的 Markdown，去掉区块图片的引用，HTML 表格转成 Markdown 表格；纯文本与段落：按阅读顺序拼接文字块，页眉页脚、图片、图表、印章等不计入 |

PP-StructureV3 明显较慢，服务端处理一张截图从几秒到几十秒不等（随服务负载变化），编辑器与面板里都会提示。

### 任务接口的流程

提交成功后面板显示「云端处理中…」；提交后约 300 ms 第一次查询任务状态，之后间隔逐次拉长，最长 2 秒一次，一个任务最多等 2 分钟。完成后下载服务给出的结果链接（JSONL）：这一步**不带令牌**，最多跟随 3 次重定向且不降级到 http，结果上限 32 MB。取消时停止查询（服务没有撤销任务的接口）。

### 连接测试

- 任务接口：`GET <地址>/wisp-connection-test`。返回 401 / 403 说明令牌无效；400 / 404 等说明令牌已通过鉴权，测试通过。不提交任何图片。
- 部署的服务：提交一张 32 × 32 的白图，`errorCode` 为 0 或「没有文字」即通过。

### 错误

先看响应体里的错误码（`code` / `errorCode`，可能是字符串），再看 HTTP 状态：

| 类别 | 常见原因 |
| --- | --- |
| 令牌无效或无权限 | 401 / 403 |
| 额度用尽 | 日额度用完（错误码 12001，或带额度字样的 429） |
| 触发限流 | 其他 429、限流类错误码 |
| 图片过大 | 413 或对应的错误码 |
| 地址或模型不存在 | 404、模型名不被接受 |
| 超时 | 408 / 504，或任务在 2 分钟内没有完成 |
| 服务端错误 | 任务失败、5xx |
| 网络错误 | 连不上服务 |

错误信息只用固定的说明，不回显服务返回的原文。日志只记录服务的主机、接口类别（任务接口或部署的服务）、状态码与耗时，不记录任务 id、结果链接与令牌。免费额度、计费与使用条款以 PaddleOCR 与 AI Studio 的官方说明为准。

## 多设备同步

开启[多设备同步](user-guide.md#多设备同步)后，服务列表、每个服务的设置（包括自定义请求头与高级选项）、启用的模型与默认模型都会同步到你的其他设备；勾选「API Key」时，Key 也会在本机加密后同步，到达后写入那台设备的系统凭据存储。

- Wisp 自动探测到的能力——哪些参数被服务拒绝过、「未确认」的模型是否支持图片——不同步，每台设备各自探测；你手动指定的识图能力会同步。
- 每个 Key 记着设置它时服务的地址（协议、主机与端口）。服务在其他设备上改到了别的主机、而 Key 没有跟着更新时，Wisp 会删掉这台设备上的旧 Key 并提示重新填写，不会把旧 Key 发往新地址。
- 自定义请求头不是 Key：它随服务的设置一起同步（同样端到端加密），不受「API Key」开关控制，也不绑定地址——服务在其他设备上改到别的主机后，请求头会随请求发往新地址。有的网关要求把凭据放在请求头里（例如 `cf-aig-authorization`）；不想让它同步到其他设备时，请关闭「设置与模型服务」的同步。
- 删除一个服务会在所有设备上删除它和它的 Key。

## 贡献者：代码在哪里

| 内容 | 位置 |
| --- | --- |
| 服务模板 | `crates/wisp-llm/src/templates.rs`（模板名的界面文案在 `apps/desktop/src/locales/*/settings.json` 的 `settings.providers.template.*`） |
| Base URL 规范化 | `crates/wisp-llm/src/url.rs`，测试向量在 `crates/wisp-llm/tests/` |
| 请求体与参数规则 | `crates/wisp-llm/src/request/`（`params.rs` 决定 temperature、最大输出、思考参数） |
| 流式解析与协议适配 | `crates/wisp-llm/src/sse.rs`、`src/adapter/` |
| 错误归一化 | `crates/wisp-llm/src/errors.rs` |
| 识图能力判定 | `crates/wisp-llm/src/vision.rs` |
| 参数降级 | `crates/wisp-llm/src/degrade.rs` |
| 提示词 | `crates/wisp-prompts/templates/`；离线翻译（Index-Translate）的提示词格式在 `crates/wisp-prompts/src/index.rs` |
| PaddleOCR 客户端（地址判别、两种接口、结果解析、错误分类、连接测试） | `crates/wisp-local/src/ocr/cloud.rs` 与 `src/ocr/cloud/` |

协议适配与错误归一化都有基于 SSE 样本的回放测试（`cargo test -p wisp-llm`），不需要真实的 Key；PaddleOCR 客户端的测试用 wiremock 模拟两种接口（`cargo test -p wisp-local --test cloud_wiremock`），对真实服务的测试默认忽略（见[开发指南](development.md#离线模型与-paddleocr-的真机测试)）。新增模板时：在 `templates.rs` 里加一项，给 zh-CN 与 en-US 两份 locales 补上模板名，运行 `pnpm i18n:check` 与 `cargo test -p wisp-llm`。
