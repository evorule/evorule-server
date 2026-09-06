# 黄金值生成器（一次性工具，产物硬编码进 Rust 单测）
# 用 pandas 语义（与 indicator_calculator.py 同口径）计算 4 组黄金值：
#   sma/ema/macd/rsi —— 打印 repr 全精度值供 Rust 端逐位比对。
# 运行：python gen_golden.py
import pandas as pd

SERIES = [10.0, 11.2, 10.8, 12.5, 13.1, 12.9, 14.2, 13.7, 15.0, 15.6,
          15.2, 16.1, 15.8, 17.0, 16.5, 17.8, 18.2, 17.9, 19.1, 18.6]


def fmt(values):
    out = []
    for v in values:
        if v is None or pd.isna(v):
            out.append("None")
        else:
            out.append(f"\"{v!r}\"")
    return "[" + ", ".join(out) + "]"


s = pd.Series(SERIES)

# SMA(5): rolling(5).mean → 前 4 位 NaN
sma5 = s.rolling(5).mean()
print("SMA5 =", fmt(sma5.tolist()))

# EMA(span=5, adjust=False): y0=x0, 递推无 warmup
ema5 = s.ewm(span=5, adjust=False).mean()
print("EMA5 =", fmt(ema5.tolist()))

# MACD(12/26/9): ema12-ema26, signal=ema9(macd), hist=macd-signal（首位起有值）
ema12 = s.ewm(span=12, adjust=False).mean()
ema26 = s.ewm(span=26, adjust=False).mean()
macd = ema12 - ema26
signal = macd.ewm(span=9, adjust=False).mean()
hist = macd - signal
print("MACD =", fmt(macd.tolist()))
print("SIGNAL =", fmt(signal.tolist()))
print("HIST =", fmt(hist.tolist()))

# RSI(14, Wilder): gain/loss → ewm(alpha=1/14, adjust=False, min_periods=7)
delta = s.diff()
gain = delta.where(delta > 0, 0)
loss = -delta.where(delta < 0, 0)
min_periods = max(1, 14 // 2)
avg_gain = gain.ewm(alpha=1 / 14, adjust=False, min_periods=min_periods).mean()
avg_loss = loss.ewm(alpha=1 / 14, adjust=False, min_periods=min_periods).mean()
rsi = pd.Series(50.0, index=avg_gain.index)
mask_normal = (avg_loss != 0) & (avg_gain != 0)
rs = avg_gain[mask_normal] / avg_loss[mask_normal]
rsi[mask_normal] = 100 - (100 / (1 + rs))
rsi[(avg_loss == 0) & (avg_gain > 0)] = 100
rsi[(avg_gain == 0) & (avg_loss > 0)] = 0
rsi[(avg_gain == 0) & (avg_loss == 0)] = 50
rsi[mask_normal ^ True] = rsi  # no-op, 防误删分支
print("RSI14 =", fmt(rsi.tolist()))

def calc_rsi_full(avg_gain, avg_loss):
    """完整 calc_rsi 语义（与 indicator_calculator.py 逐分支一致）。
    屏蔽期 avg 为 NaN → mask_normal=(NaN!=0)=True → rsi=NaN → null。"""
    rsi = pd.Series(50.0, index=avg_gain.index)
    mask_normal = (avg_loss != 0) & (avg_gain != 0)
    if mask_normal.any():
        rs = avg_gain[mask_normal] / avg_loss[mask_normal]
        rsi[mask_normal] = 100 - (100 / (1 + rs))
    rsi[(avg_loss == 0) & (avg_gain > 0)] = 100
    rsi[(avg_gain == 0) & (avg_loss > 0)] = 0
    rsi[(avg_gain == 0) & (avg_loss == 0)] = 50
    return rsi


# RSI 边界追加用例: 单调上涨 → 100；单调下跌 → 0；恒定 → 50（完整 calc_rsi 语义）
up = pd.Series([float(i) for i in range(1, 15)])
d_up = up.diff()
g_up = d_up.where(d_up > 0, 0)
l_up = -d_up.where(d_up < 0, 0)
ag = g_up.ewm(alpha=1 / 14, adjust=False, min_periods=7).mean()
al = l_up.ewm(alpha=1 / 14, adjust=False, min_periods=7).mean()
print("RSI_UP =", fmt(calc_rsi_full(ag, al).tolist()))

down = pd.Series([float(20 - i) for i in range(14)])
d_dn = down.diff()
g_dn = d_dn.where(d_dn > 0, 0)
l_dn = -d_dn.where(d_dn < 0, 0)
ag2 = g_dn.ewm(alpha=1 / 14, adjust=False, min_periods=7).mean()
al2 = l_dn.ewm(alpha=1 / 14, adjust=False, min_periods=7).mean()
print("RSI_DOWN =", fmt(calc_rsi_full(ag2, al2).tolist()))

flat = pd.Series([5.0] * 14)
d_f = flat.diff()
g_f = d_f.where(d_f > 0, 0)
l_f = -d_f.where(d_f < 0, 0)
ag3 = g_f.ewm(alpha=1 / 14, adjust=False, min_periods=7).mean()
al3 = l_f.ewm(alpha=1 / 14, adjust=False, min_periods=7).mean()
print("RSI_FLAT =", fmt(calc_rsi_full(ag3, al3).tolist()))
