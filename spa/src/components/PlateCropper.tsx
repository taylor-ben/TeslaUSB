import { useCallback, useEffect, useRef, useState } from "preact/hooks";
import type { JSX } from "preact";
import {
  centeredMaxRect,
  clampMoveRect,
  defaultRegion,
  deriveBaseName,
  resizeRectFromCorner,
  targetFor,
  type CropCorner,
  type CropRect,
  type PlateRegion,
} from "../lib/plateCrop";
import "../styles/plate-cropper.css";

export interface PlateCropRequest { file: File; img: HTMLImageElement; }
export interface PlateCropResult { name: string; blob: Blob; }

interface PlateCropperProps {
  request: PlateCropRequest | null;
  onConfirm: (result: PlateCropResult) => void;
  onCancel: () => void;
}

type DragState =
  | { mode: "move"; startPt: { x: number; y: number }; startRect: CropRect }
  | { mode: "resize"; corner: CropCorner; startPt: { x: number; y: number }; startRect: CropRect };

const HANDLE_RADIUS = 10;
const HANDLE_SIZE = 8;

function pointInRect(x: number, y: number, r: CropRect): boolean {
  return x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h;
}

export function PlateCropper({ request, onConfirm, onCancel }: PlateCropperProps) {
  const [region, setRegion] = useState<PlateRegion>("na");
  const [baseName, setBaseName] = useState("plate");
  const [rect, setRect] = useState<CropRect>({ x: 0, y: 0, w: 0, h: 0 });
  const [drag, setDrag] = useState<DragState | null>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const previewRef = useRef<HTMLCanvasElement>(null);

  const img = request?.img ?? null;
  const imgW = img?.naturalWidth ?? 1;
  const imgH = img?.naturalHeight ?? 1;
  const displayW = Math.min(imgW, 520);
  const displayH = Math.max(1, Math.round(displayW * imgH / imgW));
  const canvasScale = imgW / displayW;
  const target = targetFor(region);

  const resetRect = useCallback((nextRegion: PlateRegion) => {
    if (!img) return;
    setRect(centeredMaxRect(img.naturalWidth, img.naturalHeight, targetFor(nextRegion)));
  }, [img]);

  useEffect(() => {
    if (!request) return;
    const nextRegion = defaultRegion(request.img.naturalWidth, request.img.naturalHeight);
    setRegion(nextRegion);
    setBaseName(deriveBaseName(request.file.name));
    setRect(centeredMaxRect(request.img.naturalWidth, request.img.naturalHeight, targetFor(nextRegion)));
    setDrag(null);
  }, [request]);

  const drawMain = useCallback(() => {
    if (!img) return;
    const canvas = canvasRef.current;
    if (!canvas) return;
    canvas.width = displayW;
    canvas.height = displayH;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.clearRect(0, 0, displayW, displayH);
    ctx.drawImage(img, 0, 0, displayW, displayH);

    const dx = rect.x / canvasScale;
    const dy = rect.y / canvasScale;
    const dw = rect.w / canvasScale;
    const dh = rect.h / canvasScale;

    ctx.fillStyle = "rgba(0, 0, 0, 0.5)";
    ctx.fillRect(0, 0, displayW, dy);
    ctx.fillRect(0, dy, dx, dh);
    ctx.fillRect(dx + dw, dy, displayW - dx - dw, dh);
    ctx.fillRect(0, dy + dh, displayW, displayH - dy - dh);

    ctx.strokeStyle = "#48d597";
    ctx.lineWidth = 2;
    ctx.strokeRect(dx, dy, dw, dh);

    ctx.fillStyle = "#ffffff";
    const corners = [
      [dx, dy],
      [dx + dw, dy],
      [dx, dy + dh],
      [dx + dw, dy + dh],
    ];
    for (const [cx, cy] of corners) {
      ctx.fillRect(cx - HANDLE_SIZE / 2, cy - HANDLE_SIZE / 2, HANDLE_SIZE, HANDLE_SIZE);
      ctx.strokeRect(cx - HANDLE_SIZE / 2, cy - HANDLE_SIZE / 2, HANDLE_SIZE, HANDLE_SIZE);
    }
  }, [img, displayW, displayH, rect, canvasScale]);

  const drawPreview = useCallback(() => {
    if (!img) return;
    const canvas = previewRef.current;
    if (!canvas) return;
    canvas.width = target.width;
    canvas.height = target.height;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.imageSmoothingEnabled = true;
    ctx.imageSmoothingQuality = "high";
    ctx.clearRect(0, 0, target.width, target.height);
    ctx.drawImage(img, rect.x, rect.y, rect.w, rect.h, 0, 0, target.width, target.height);
  }, [img, rect, target]);

  useEffect(() => {
    drawMain();
    drawPreview();
  }, [drawMain, drawPreview, region]);

  useEffect(() => {
    if (!request) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onCancel();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [request, onCancel]);

  const toCanvasPoint = useCallback((event: PointerEvent | JSX.TargetedPointerEvent<HTMLCanvasElement>) => {
    const canvas = canvasRef.current;
    if (!canvas) return { x: 0, y: 0 };
    const bounds = canvas.getBoundingClientRect();
    const x = ((event.clientX - bounds.left) * canvas.width) / bounds.width;
    const y = ((event.clientY - bounds.top) * canvas.height) / bounds.height;
    return { x, y };
  }, []);

  const onPointerDown = useCallback((event: JSX.TargetedPointerEvent<HTMLCanvasElement>) => {
    if (!img || !canvasRef.current) return;
    const pt = toCanvasPoint(event);
    const rectDisplay = {
      x: rect.x / canvasScale,
      y: rect.y / canvasScale,
      w: rect.w / canvasScale,
      h: rect.h / canvasScale,
    };
    const corners: Array<{ corner: CropCorner; x: number; y: number }> = [
      { corner: "tl", x: rectDisplay.x, y: rectDisplay.y },
      { corner: "tr", x: rectDisplay.x + rectDisplay.w, y: rectDisplay.y },
      { corner: "bl", x: rectDisplay.x, y: rectDisplay.y + rectDisplay.h },
      { corner: "br", x: rectDisplay.x + rectDisplay.w, y: rectDisplay.y + rectDisplay.h },
    ];
    for (const c of corners) {
      if (Math.hypot(pt.x - c.x, pt.y - c.y) <= HANDLE_RADIUS) {
        setDrag({ mode: "resize", corner: c.corner, startPt: pt, startRect: rect });
        canvasRef.current.setPointerCapture(event.pointerId);
        event.preventDefault();
        return;
      }
    }
    if (pointInRect(pt.x, pt.y, rectDisplay)) {
      setDrag({ mode: "move", startPt: pt, startRect: rect });
      canvasRef.current.setPointerCapture(event.pointerId);
      event.preventDefault();
    }
  }, [img, rect, canvasScale, toCanvasPoint]);

  const onPointerMove = useCallback((event: JSX.TargetedPointerEvent<HTMLCanvasElement>) => {
    if (!drag || !img) return;
    const pt = toCanvasPoint(event);
    const dxSrc = (pt.x - drag.startPt.x) * canvasScale;
    const dySrc = (pt.y - drag.startPt.y) * canvasScale;
    if (drag.mode === "move") {
      setRect(clampMoveRect(drag.startRect, dxSrc, dySrc, img.naturalWidth, img.naturalHeight));
    } else {
      const resized = resizeRectFromCorner(
        drag.startRect,
        drag.corner,
        dxSrc,
        dySrc,
        img.naturalWidth,
        img.naturalHeight,
        target.width / target.height,
      );
      if (resized) setRect(resized);
    }
  }, [drag, img, canvasScale, target, toCanvasPoint]);

  const onPointerUp = useCallback((event: JSX.TargetedPointerEvent<HTMLCanvasElement>) => {
    if (canvasRef.current?.hasPointerCapture(event.pointerId)) {
      canvasRef.current.releasePointerCapture(event.pointerId);
    }
    setDrag(null);
  }, []);

  const onConfirmClick = useCallback(() => {
    if (!img) return;
    const out = document.createElement("canvas");
    out.width = target.width;
    out.height = target.height;
    const ctx = out.getContext("2d");
    if (!ctx) return;
    ctx.imageSmoothingEnabled = true;
    ctx.imageSmoothingQuality = "high";
    ctx.drawImage(img, rect.x, rect.y, rect.w, rect.h, 0, 0, target.width, target.height);
    out.toBlob((blob) => {
      if (blob) onConfirm({ name: `${baseName}.png`, blob });
    }, "image/png");
  }, [img, target, rect, baseName, onConfirm]);

  if (!request || !img) return null;

  return (
    <div class="plate-cropper-backdrop" onClick={onCancel}>
      <div
        class="plate-cropper"
        data-testid="plate-cropper"
        role="dialog"
        aria-label="Crop license plate"
        onClick={(e) => e.stopPropagation()}
      >
        <canvas
          ref={canvasRef}
          data-testid="plate-cropper-canvas"
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          onPointerCancel={onPointerUp}
        />
        <div class="plate-cropper-regions">
          <label>
            <input
              data-testid="plate-cropper-region-na"
              type="radio"
              name="plate-region"
              checked={region === "na"}
              onChange={() => {
                setRegion("na");
                resetRect("na");
              }}
            />
            {" "}
            North America (420×200)
          </label>
          <label>
            <input
              data-testid="plate-cropper-region-eu"
              type="radio"
              name="plate-region"
              checked={region === "eu"}
              onChange={() => {
                setRegion("eu");
                resetRect("eu");
              }}
            />
            {" "}
            Europe (420×100)
          </label>
        </div>
        <div class="plate-cropper-preview-wrap">
          <canvas ref={previewRef} data-testid="plate-cropper-preview" />
        </div>
        <div class="plate-cropper-output" data-testid="plate-cropper-output">
          Output {target.width}x{target.height} PNG · {baseName}.png
        </div>
        <div class="plate-cropper-actions">
          <button class="action-btn" data-testid="plate-cropper-reset" onClick={() => resetRect(region)}>
            Reset / Maximize
          </button>
          <button class="action-btn" data-testid="plate-cropper-confirm" onClick={onConfirmClick}>
            Use this crop
          </button>
          <button class="action-btn" data-testid="plate-cropper-cancel" onClick={onCancel}>
            Cancel
          </button>
        </div>
      </div>
    </div>
  );
}
