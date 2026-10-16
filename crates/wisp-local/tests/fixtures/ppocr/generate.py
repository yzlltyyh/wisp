"""用官方 PaddleOCR 3.7 产线（ONNX Runtime 后端）生成 PP-OCRv6 黄金样例。"""
import json, os, sys, time
os.environ.setdefault("ORT_DISABLE_TELEMETRY", "1")
os.environ.setdefault("PADDLE_PDX_DISABLE_MODEL_SOURCE_CHECK", "True")
from paddleocr import PaddleOCR
import paddleocr, paddlex, onnxruntime

models, tier, out_dir = sys.argv[1], sys.argv[2], sys.argv[3]
images = sys.argv[4:]
ocr = PaddleOCR(
    text_detection_model_name=f"PP-OCRv6_{tier}_det",
    text_detection_model_dir=os.path.join(models, f"PP-OCRv6_{tier}_det_onnx"),
    text_recognition_model_name=f"PP-OCRv6_{tier}_rec",
    text_recognition_model_dir=os.path.join(models, f"PP-OCRv6_{tier}_rec_onnx"),
    use_doc_orientation_classify=False,
    use_doc_unwarping=False,
    use_textline_orientation=False,
    text_det_limit_side_len=64,
    text_det_limit_type="min",
    text_det_thresh=0.3,
    text_det_box_thresh=0.6,
    text_det_unclip_ratio=1.5,
    text_rec_score_thresh=0.0,
    text_recognition_batch_size=1,
    engine="onnxruntime",
)
for img in images:
    t = time.time()
    res = ocr.predict(img)[0]
    dt = time.time() - t
    r = res.json["res"] if hasattr(res, "json") else res
    lines = [
        {"box": [[int(x), int(y)] for x, y in poly], "text": text, "score": float(score)}
        for poly, text, score in zip(r["rec_polys"], r["rec_texts"], r["rec_scores"])
    ]
    doc = {
        "source": os.path.basename(img),
        "generator": f"paddleocr {paddleocr.__version__} / paddlex {paddlex.__version__} / onnxruntime {onnxruntime.__version__}",
        "params": {"limit_side_len": 64, "limit_type": "min", "max_side_limit": r["text_det_params"]["max_side_limit"],
                   "thresh": 0.3, "box_thresh": 0.6, "unclip_ratio": 1.5, "rec_batch_size": 1,
                   "textline_orientation": False, "doc_preprocessor": False},
        "lines": lines,
    }
    name = os.path.splitext(os.path.basename(img))[0]
    with open(os.path.join(out_dir, f"{name}.{tier}.json"), "w") as f:
        json.dump(doc, f, ensure_ascii=False, indent=1)
    print(name, tier, len(lines), f"{dt:.2f}s")
