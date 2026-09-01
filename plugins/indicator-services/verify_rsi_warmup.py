# 验证 RSI warmup 语义: min_periods 屏蔽 vs 分类 mask 交互
import pandas as pd

s = pd.Series([float(i) for i in range(1, 15)])  # 单调上涨
delta = s.diff()
gain = delta.where(delta > 0, 0)
loss = -delta.where(delta < 0, 0)
ag = gain.ewm(alpha=1 / 14, adjust=False, min_periods=7).mean()
al = loss.ewm(alpha=1 / 14, adjust=False, min_periods=7).mean()
print("avg_gain[:8] =", ag.head(8).tolist())
print("avg_loss[:8] =", al.head(8).tolist())
print("mask_normal[:8] =", ((al != 0) & (ag != 0)).head(8).tolist())
print("mask_up[:8] =", ((al == 0) & (ag > 0)).head(8).tolist())
