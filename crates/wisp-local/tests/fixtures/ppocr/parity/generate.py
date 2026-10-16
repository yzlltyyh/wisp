"""生成 OpenCV / pyclipper 语义对齐单测的参考数据（官方 PaddleX 3.7 产线所用的同一批函数）。

输出 `parity.json`：缩放、检测输入尺寸、识别输入宽度、最小外接矩形、外扩、透视裁剪与 DB 后处理的输入与官方结果。
图像与概率图都是固定种子的小尺寸合成数据，字节缓冲区用 base64 存放。
"""
import base64, json, os, sys

import cv2
import numpy as np
import paddlex
from paddlex.inference.models.text_detection.processors import DBPostProcess, DetResizeForTest
from paddlex.inference.models.text_recognition.processors import OCRReisizeNormImg
from paddlex.inference.pipelines.components.common.crop_image_regions import CropByPolys

rng = np.random.default_rng(20261016)


def b64(a):
    return base64.b64encode(np.ascontiguousarray(a).tobytes()).decode()


def f(v):
    # f32 → 能精确还原的十进制
    return float(np.float32(v))


def flat(points, conv=f):
    # 点列展平成 [x0, y0, x1, y1, …]（prettier 会把数字数组按行填满，嵌套的二元组则每个占好几行）
    return [conv(c) for p in points for c in p]


def smooth_image(h, w):
    """噪声叠加平滑渐变：既有大量取整边界，也有接近真实图像的局部相关性"""
    yy, xx = np.mgrid[0:h, 0:w]
    base = np.stack([(xx * 7 + yy * 3) % 256, (xx * 2 + yy * 11) % 256, (xx * yy) % 256], axis=-1)
    noise = rng.integers(-40, 41, size=(h, w, 3))
    return np.clip(base + noise, 0, 255).astype(np.uint8)


out = {"generator": f"opencv {cv2.__version__} / paddlex {paddlex.__version__} / numpy {np.__version__}"}

# 1. cv2.resize 默认 INTER_LINEAR（uint8 三通道）：放大、缩小、恰好 2 倍（走 INTER_AREA 快速路径）、只改一个方向
resize_cases = []
for (h, w) in [(23, 37), (40, 40), (9, 31)]:
    src = rng.integers(0, 256, size=(h, w, 3), dtype=np.uint8) if (h + w) % 2 else smooth_image(h, w)
    targets = []
    for (dh, dw) in [(17, 29), (20, 20), (h, w + 3), (h + 3, w), (5, 3), (48, 13), (11, 61)]:
        if (dh, dw) != (h, w):
            targets.append({"dh": dh, "dw": dw, "dst": b64(cv2.resize(src, (dw, dh)))})
    resize_cases.append({"h": h, "w": w, "src": b64(src), "targets": targets})
out["resize"] = resize_cases

# 2. 检测输入尺寸（DetResizeForTest type0：limit 64 / min，max_side_limit 可变）
det_sizes = []
for (h, w, limit, ltype, maxs) in [
    (528, 896, 64, "min", 4000), (846, 1200, 64, "min", 2048), (1050, 1200, 64, "min", 2048),
    (16, 100, 64, "min", 4000), (30, 20, 64, "min", 4000), (48, 1500, 64, "min", 4000),
    (2000, 5000, 64, "min", 4000), (1440, 2560, 64, "min", 2048), (720, 1280, 960, "max", 4000),
    (80, 80, 64, "min", 4000), (176, 3000, 64, "min", 2048), (49, 3, 64, "min", 4000),
]:
    op = DetResizeForTest(limit_side_len=limit, limit_type=ltype, max_side_limit=maxs)
    img = np.zeros((h, w, 3), np.uint8)
    res, shape = op.resize(img, limit, ltype, maxs)
    det_sizes.append({"h": h, "w": w, "limit": limit, "type": ltype, "max_side": maxs,
                      "rh": int(res.shape[0]), "rw": int(res.shape[1]),
                      "ratio_h": float(shape[2]), "ratio_w": float(shape[3])})
out["det_sizes"] = det_sizes

# 3. 识别输入：张量宽度与缩放宽度（逐行，batch 1）
rec = OCRReisizeNormImg()
rec_widths = []
for (h, w) in [(48, 320), (45, 300), (20, 133), (30, 7), (12, 600), (33, 2300), (10, 900), (31, 206), (47, 313), (3, 2)]:
    t = rec.resize(np.full((h, w, 3), 255, np.uint8))
    filled = int((t[0, 0, :] > 0.5).sum())
    rec_widths.append({"h": h, "w": w, "tensor_w": int(t.shape[2]), "resized_w": filled})
out["rec_widths"] = rec_widths

# 4. cv2.minAreaRect + cv2.boxPoints
rects = []
for i in range(32):
    n = int(rng.integers(3, 25))
    if i % 2 == 0:
        pts = rng.integers(-5, 120, size=(n, 2)).astype(np.int32)
        if i % 4 == 0:  # 近似轴对齐的条状点集（DB 轮廓的常见形态）
            x0, y0 = rng.integers(0, 50, size=2)
            ww, hh = rng.integers(10, 90), rng.integers(4, 20)
            pts = np.array([[x0 + rng.integers(0, ww), y0 + rng.integers(0, hh)] for _ in range(n)]
                           + [[x0, y0], [x0 + ww, y0], [x0 + ww, y0 + hh], [x0, y0 + hh]], np.int32)
    else:
        pts = (rng.random((n, 2)) * 100 - 3).astype(np.float32)
    r = cv2.minAreaRect(pts)
    bp = cv2.boxPoints(r)
    rects.append({"int": bool(pts.dtype == np.int32), "points": flat(pts.tolist()),
                  "center": [f(r[0][0]), f(r[0][1])], "size": [f(r[1][0]), f(r[1][1])], "angle": f(r[2]),
                  "box": flat(bp.tolist())})
out["min_area_rect"] = rects

# 5. DBPostProcess.unclip（pyclipper：坐标截断成整数、JT_ROUND、ET_CLOSEDPOLYGON）
post = DBPostProcess(thresh=0.3, box_thresh=0.6, max_candidates=1000, unclip_ratio=1.5)
unclips = []
for i in range(32):
    cx, cy = rng.random(2) * 200
    w, h = 4 + rng.random() * 120, 2 + rng.random() * 30
    ang = 0.0 if i % 3 == 0 else float(rng.normal(0, 8 if i % 3 == 1 else 40))
    box = cv2.boxPoints(((cx, cy), (w, h), ang))
    if i % 5 == 0:
        box = np.round(box).astype(np.float32)
    ratio = 1.5 if i % 2 == 0 else 2.0
    expanded = post.unclip(box, ratio)
    unclips.append({"box": flat(box.tolist()), "ratio": ratio,
                    "paths": [flat(p, int) for p in (expanded if expanded.ndim == 3 else [expanded])]})
out["unclip"] = unclips

# 6. 透视裁剪（CropByPolys(quad)：minAreaRect → boxPoints → get_rotate_crop_image）
crop_src = smooth_image(56, 96)
crops = []
quads = [
    [[10, 10], [60, 10], [60, 24], [10, 24]],
    [[3, 5], [70, 2], [71, 18], [4, 21]],
    [[20, 36], [80, 26], [83, 40], [23, 50]],
    [[40, 2], [55, 2], [55, 54], [40, 54]],
    [[0, 0], [96, 0], [96, 12], [0, 12]],
    [[80, 42], [96, 41], [96, 56], [80, 56]],
    [[5, 30], [30, 5], [40, 15], [15, 40]],
    [[12, 40], [44, 39], [45, 48], [12, 49]],
]
for q in quads:
    q = np.array(q, np.int16)
    img = CropByPolys(det_box_type="quad")(crop_src, [q])[0]
    crops.append({"quad": flat(q.tolist(), int), "h": int(img.shape[0]), "w": int(img.shape[1]),
                  "crop": b64(np.ascontiguousarray(img))})
out["crop"] = {"h": 56, "w": 96, "src": b64(crop_src), "cases": crops}

# 7. DB 后处理端到端（boxes_from_bitmap：轮廓 → mini box → box_score_fast → unclip → mini box → 缩放回原图）
db_cases = []
for (mh, mw, dest_h, dest_w, nblobs) in [(64, 96, 64, 96, 6), (64, 96, 61, 93, 7), (96, 64, 200, 130, 6), (64, 128, 30, 61, 9), (96, 96, 90, 100, 10)]:
    acc = np.zeros((mh, mw), np.float32)
    for k in range(nblobs):
        # 大致按文本行排布：两栏、逐行，偶尔有倾斜或竖排的块
        col, row = k % 2, k // 2
        rows = (nblobs + 1) // 2
        cy = (row + 0.5) * mh / rows + rng.normal(0, 1.0)
        w, h = 6 + rng.random() * mw * 0.35, 3 + rng.random() * min(7, mh / rows * 0.5)
        cx = (col + 0.5) * mw / 2 + rng.normal(0, 3)
        ang = float(rng.normal(0, 3)) if rng.random() < 0.8 else float(rng.random() * 180)
        poly = cv2.boxPoints(((cx, cy), (w, h), ang))
        layer = np.zeros((mh, mw), np.float32)
        cv2.fillPoly(layer, [np.round(poly).astype(np.int32)], float(0.62 + rng.random() * 0.38))
        acc = np.maximum(acc, layer)
    acc = cv2.GaussianBlur(acc, (3, 3), 0.8)
    acc = np.clip(acc + rng.normal(0, 0.03, acc.shape).astype(np.float32), 0, 1)
    q = np.round(acc * 255).astype(np.uint8)
    pred = q.astype(np.float32) / np.float32(255.0)
    # img_shape 与产线一样是 float64 数组（DetResizeForTest 返回 np.array）：缩放回原图的乘法因此是双精度
    boxes, scores = post.process(pred[None], np.array([dest_h, dest_w, mh / dest_h, mw / dest_w]), 0.3, 0.6, 1.5)
    db_cases.append({"h": mh, "w": mw, "dest_h": dest_h, "dest_w": dest_w, "pred_u8": b64(q),
                     "boxes": [flat(b.tolist(), int) for b in boxes], "scores": [float(s) for s in scores]})
out["db"] = db_cases

path = os.path.join(sys.argv[1] if len(sys.argv) > 1 else ".", "parity.json")
with open(path, "w") as fh:
    json.dump(out, fh, ensure_ascii=False, separators=(",", ":"))
print(path, os.path.getsize(path), "bytes;", {k: len(v) if isinstance(v, list) else "" for k, v in out.items()})
