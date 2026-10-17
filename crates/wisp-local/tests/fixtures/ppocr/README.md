# PP-OCRv6 黄金样例

`ocr_live` 测试把本地 PP-OCRv6（oar-ocr + ONNX Runtime，经 ORT 工作进程）的识别结果与官方 Python 产线的输出比对：
同一张图、同一档模型，逐行比较文字与框。严格指标是「文字一致且框的 IoU ≥ 0.9」，另报四个顶点完全相同的行数、
「文字一致且 IoU ≥ 0.5」、检测召回与多检。预处理与后处理已与官方产线逐步对齐（见 `parity/` 与 `ocr_opencv_parity`），
同一份像素输入时两者的文字与框完全相同。

这些 JSON（含低分行）也是文本拼装黄金输出（`tests/assemble_golden.rs`，输出在 `../assemble/`）的输入：重新生成后，
连同 `assemble/` 的差异一起审阅，确认后用 `UPDATE_GOLDEN=1 cargo test -p wisp-local --test assemble_golden` 重写。

## 文件

| 文件 | 内容 |
|---|---|
| `<图片名>.<tiny\|small>.json` | 官方产线的输出：`lines[].{box, text, score}`（`box` 为四个顶点的整数坐标），`generator` 记录生成时的版本，`params` 记录产线参数 |
| `general_ocr_002.jpg` | PaddleOCR 仓库 v3.7.0 的 iOS 演示样例图（`deploy/ios_demo/PaddleOCRDemo/Resources/SampleImages/general_ocr_002.jpg`，Apache-2.0，sha256 `4425af33dd163cf73bdff502bd35ee527e9bdd5725501db1da78bfdae9f538f4`） |
| `generate.py` | 黄金样例的生成脚本 |
| `parity/parity.json`、`parity/generate.py` | 逐像素 / 逐框对齐单测（`tests/ocr_opencv_parity.rs`）的参考数据与生成脚本：固定种子的小尺寸合成图与概率图，经官方产线所用的同一批函数（`cv2.resize`、`DetResizeForTest`、`OCRReisizeNormImg`、`cv2.minAreaRect` / `cv2.boxPoints`、`DBPostProcess.unclip`、`CropByPolys`、`DBPostProcess`）得到的结果 |

其余图片是仓库自带的截图 `docs/images/*.png`（translate-light、ocr-dark、settings-providers-light、clipboard-light、dictionary-dark）。

`general_ocr_002` 的黄金样例以测试实际交给识别的像素为输入生成：测试用 `image` 解码 JPEG，它与 OpenCV（libjpeg-turbo）
解出的像素有几个灰度级的差异（9% 的像素，最大 4），这与识别产线无关，所以先用 `export_decoded_inputs_for_the_generator`
把解码结果写成 PNG，再交给官方产线（JSON 里的 `source` 因此是 `general_ocr_002.png`）。

## 参数

与 Wisp 的本地识别一致：检测 `limit_side_len 64 / limit_type min`、`thresh 0.3 / box_thresh 0.6 / unclip_ratio 1.5`，
识别逐行（batch 1）、不过滤低分行；不启用文档方向、矫正与文本行方向分类。模型为官方 ONNX 文件
（`PaddlePaddle/PP-OCRv6_{tiny,small}_{det,rec}_onnx`）。

## 重新生成

```bash
python3 -m venv /tmp/ppocr-venv
/tmp/ppocr-venv/bin/pip install "paddleocr==3.7.0" "paddlex==3.7.2" "onnxruntime==1.28.0"   # 带 opencv-contrib-python 4.10.0.84
export ORT_DISABLE_TELEMETRY=1 PADDLE_PDX_CACHE_HOME=/tmp/ppocr-cache
# general_ocr_002：先导出 image 解码的像素
mkdir -p /tmp/ppocr-input
WISP_PPOCR_EXPORT_DIR=/tmp/ppocr-input cargo test -p wisp-local --features ort --test ocr_live -- --ignored export_decoded
# <models> 下是官方仓库的目录：PP-OCRv6_tiny_det_onnx/{inference.onnx,inference.yml} 等
/tmp/ppocr-venv/bin/python -I generate.py <models> tiny . /tmp/ppocr-input/general_ocr_002.png ../../../../../docs/images/translate-light.png …
/tmp/ppocr-venv/bin/python -I generate.py <models> small . …
# 对齐单测的参考数据（不需要模型）
/tmp/ppocr-venv/bin/python -I parity/generate.py parity
# 生成的 JSON 交给 prettier 统一格式（仓库的 format:check 会检查）
pnpm exec prettier --write .
```
