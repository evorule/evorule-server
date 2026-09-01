# 验证 pandas ewm(adjust=False) 的浮点递推形态,确定 Rust 侧逐位对齐公式
import pandas as pd

SERIES = [10.0, 11.2, 10.8, 12.5, 13.1, 12.9, 14.2, 13.7, 15.0, 15.6,
          15.2, 16.1, 15.8, 17.0, 16.5, 17.8, 18.2, 17.9, 19.1, 18.6]
s = pd.Series(SERIES)
ref = s.ewm(span=5, adjust=False).mean()

# 候选 A: y = (1-a)*y + a*x
a = 2.0 / (5 + 1)
y = [SERIES[0]]
for x in SERIES[1:]:
    y.append((1 - a) * y[-1] + a * x)
print("A match:", y == ref.tolist())
print("A[17]:", repr(y[17]), " ref:", repr(ref[17]))

# 候选 B: y = y + a*(x - y)
y2 = [SERIES[0]]
for x in SERIES[1:]:
    y2.append(y2[-1] + a * (x - y2[-1]))
print("B match:", y2 == ref.tolist())
print("B[17]:", repr(y2[17]))
