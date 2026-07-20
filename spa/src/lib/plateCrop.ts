export const PLATE_NA = { width: 420, height: 200 } as const;
export const PLATE_EU = { width: 420, height: 100 } as const;
export const MAX_FILENAME_LENGTH = 32;
export const MAX_PLATE_BYTES = 512 * 1024;

export type PlateRegion = "na" | "eu";
export interface PlateTarget { width: number; height: number; }
export interface CropRect { x: number; y: number; w: number; h: number; }
export type CropCorner = "tl" | "tr" | "bl" | "br";

export function targetFor(region: PlateRegion): PlateTarget {
  return region === "eu" ? { ...PLATE_EU } : { ...PLATE_NA };
}

function clamp(v: number, min: number, max: number): number {
  return Math.max(min, Math.min(max, v));
}

export function deriveBaseName(originalFilename: string): string {
  const base = originalFilename
    .replace(/\.[^.]+$/, "")
    .replace(/[^A-Za-z0-9]/g, "")
    .slice(0, MAX_FILENAME_LENGTH);
  return base || "plate";
}

export function isCompliantPng(type: string, w: number, h: number): boolean {
  if (type !== "image/png") return false;
  return (w === PLATE_NA.width && h === PLATE_NA.height)
    || (w === PLATE_EU.width && h === PLATE_EU.height);
}

export function defaultRegion(naturalW: number, naturalH: number): PlateRegion {
  const srcAspect = naturalW / naturalH;
  const naDiff = Math.abs(srcAspect - (PLATE_NA.width / PLATE_NA.height));
  const euDiff = Math.abs(srcAspect - (PLATE_EU.width / PLATE_EU.height));
  return euDiff < naDiff ? "eu" : "na";
}

export function centeredMaxRect(imgW: number, imgH: number, target: PlateTarget): CropRect {
  const targetAspect = target.width / target.height;
  let cw = imgW;
  let ch = Math.round(cw / targetAspect);
  if (ch > imgH) {
    ch = imgH;
    cw = Math.round(ch * targetAspect);
  }
  return {
    x: Math.round((imgW - cw) / 2),
    y: Math.round((imgH - ch) / 2),
    w: cw,
    h: ch,
  };
}

export function clampMoveRect(startRect: CropRect, dxSrc: number, dySrc: number, imgW: number, imgH: number): CropRect {
  const nx = clamp(startRect.x + dxSrc, 0, imgW - startRect.w);
  const ny = clamp(startRect.y + dySrc, 0, imgH - startRect.h);
  return {
    x: Math.round(nx),
    y: Math.round(ny),
    w: startRect.w,
    h: startRect.h,
  };
}

export function resizeRectFromCorner(
  startRect: CropRect,
  corner: CropCorner,
  dxSrc: number,
  dySrc: number,
  imgW: number,
  imgH: number,
  targetAspect: number,
): CropRect | null {
  const anchorX = (corner === "tr" || corner === "br") ? startRect.x : startRect.x + startRect.w;
  const anchorY = (corner === "bl" || corner === "br") ? startRect.y : startRect.y + startRect.h;
  const px = startRect.x + (corner.endsWith("l") ? dxSrc : startRect.w + dxSrc);
  const py = startRect.y + (corner.startsWith("t") ? dySrc : startRect.h + dySrc);
  let newW = Math.abs(px - anchorX);
  let newH = Math.abs(py - anchorY);
  if (newW / newH > targetAspect) newW = newH * targetAspect;
  else newH = newW / targetAspect;
  let nx = (corner === "tr" || corner === "br") ? anchorX : anchorX - newW;
  let ny = (corner === "bl" || corner === "br") ? anchorY : anchorY - newH;
  if (nx < 0) {
    newW += nx;
    nx = 0;
    newH = newW / targetAspect;
  }
  if (ny < 0) {
    newH += ny;
    ny = 0;
    newW = newH * targetAspect;
  }
  if (nx + newW > imgW) {
    newW = imgW - nx;
    newH = newW / targetAspect;
  }
  if (ny + newH > imgH) {
    newH = imgH - ny;
    newW = newH * targetAspect;
  }
  if (newW < 16 || newH < 16) return null;
  const x = Math.round(nx);
  const y = Math.round(ny);
  return {
    x,
    y,
    w: Math.min(Math.round(newW), imgW - x),
    h: Math.min(Math.round(newH), imgH - y),
  };
}
