# WebP stand-ins for upstream's largest PNGs

`vite.config.ts` (`WEBP_STAND_INS`) resolves these in place of the PNGs under `src/`, which stay
exactly as upstream ships them. The relay build inlines every asset as base64, so these bytes land in
the bundle's stylesheet and scripts; the PNGs made the first load about 5.7 MB heavier.

| File | Source | Made with |
| --- | --- | --- |
| `card-texture*.webp` | `src/shared/ui/assets/card-texture*.png` | `cwebp -lossless -z 9` (lossless: the nine-slice dither must stay exact) |
| `buzz-wordmark.webp` | `src/features/onboarding/assets/landing/buzz-wordmark.png` | `cwebp -q 90 -alpha_q 100 -m 6` |
| `starter-team/*.webp` | `src/features/onboarding/assets/starter-team/*.png` (APNG, 40 frames at 100 ms) | frames via `ffmpeg -f apng -i <png> -fps_mode passthrough f_%04d.png`, then `img2webp -loop 0 -lossy -q 75 -m 6 -d 100 f_*.png` |

When upstream changes one of those PNGs, regenerate its WebP the same way.
