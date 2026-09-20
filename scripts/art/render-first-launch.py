"""Offline art authoring. Production embeds the resulting literal text frames."""
import math
from pathlib import Path
out = Path(__file__).resolve().parents[2] / 'packages/tui-rs/frames/deixic'
import sys
compact = '--compact' in sys.argv
W, H = (40, 12) if compact else (48, 18)
out = out / ('compact' if compact else 'normal')
out.mkdir(parents=True, exist_ok=True)
ramp = ' .,:;=+*#%@@'
for frame in range(36):
    phase = frame / 36 * math.tau
    yaw = 0.48 + math.sin(phase) * 0.28
    tilt = -0.12 + math.cos(phase) * 0.06
    cy, sy = (math.cos(yaw), math.sin(yaw))
    ct, st = (math.cos(tilt), math.sin(tilt))

    def sdf(x, y, z):
        x, z = (cy * x - sy * z, sy * x + cy * z)
        y, z = (ct * y - st * z, st * y + ct * z)
        outer = max(-0.86 - x, abs(y) - 0.92) if x < -0.1 else math.hypot(x + 0.1, y) - 0.92
        inner = max(-0.4 - x, abs(y) - 0.48) if x < -0.1 else math.hypot(x + 0.1, y) - 0.48
        shape = max(outer, -inner)
        a, b = (shape + 0.14, abs(z) - 0.3 + 0.14)
        return min(max(a, b), 0) + math.hypot(max(a, 0), max(b, 0)) - 0.14
    rows = []
    for row in range(H):
        line = ''
        for col in range(W):
            x = (col - (W - 1) / 2) * (0.087 if compact else 0.063)
            y = ((H - 1) / 2 - row) * (0.18 if compact else 0.12)
            z = 2.8
            for step in range(65):
                distance = sdf(x, y, z)
                if distance < 0.002 or z < -1.5:
                    break
                z -= max(distance, 0.002)
            if z < -1.5:
                line += ' '
                continue
            e = 0.004
            normal = [sdf(x + e, y, z) - sdf(x - e, y, z), sdf(x, y + e, z) - sdf(x, y - e, z), sdf(x, y, z + e) - sdf(x, y, z - e)]
            length = math.sqrt(sum((n * n for n in normal)))
            normal = [n / length for n in normal]
            light = (-0.48, 0.64, 0.6)
            diffuse = max(0, sum((a * b for a, b in zip(normal, light))))
            spec = max(0, normal[0] * -0.24 + normal[1] * 0.32 + normal[2] * 0.916) ** 20
            luminance = min(0.99, 0.12 + 0.7 * diffuse + 0.22 * spec + 0.08 * math.cos(y * 2.2 + x))
            line += ramp[int(luminance * (len(ramp) - 1))]
        rows.append(line.rstrip())
    (out / f'frame_{frame + 1:02}.txt').write_text('\n'.join(rows) + '\n')
print((out / 'frame_01.txt').read_text())
