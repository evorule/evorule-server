# 临时验证脚本:Python 复刻 pandas roll_mean Cython 算法,确认与 pandas 3.0.5 逐位一致
import math
import random

import pandas as pd


def roll_mean_pandas(arr, w):
    n = len(arr)
    out = [None] * n
    sx = 0.0
    ca = 0.0  # compensation_add
    cr = 0.0  # compensation_remove
    nobs = 0
    neg = 0
    ncsv = 0
    prev = None

    def add(v):
        nonlocal sx, ca, nobs, neg, ncsv, prev
        nobs += 1
        y = v - ca
        t = sx + y
        ca = t - sx - y
        sx = t
        if math.copysign(1.0, v) < 0:
            neg += 1
        if v == prev:
            ncsv += 1
        else:
            ncsv = 1
        prev = v

    def rem(v):
        nonlocal sx, cr, nobs, neg
        nobs -= 1
        y = -v - cr
        t = sx + y
        cr = t - sx - y
        sx = t
        if math.copysign(1.0, v) < 0:
            neg -= 1

    prev_s = prev_e = 0
    for i in range(n):
        ws = max(0, i - w + 1)
        we = i + 1
        if i == 0 or ws >= prev_e:
            sx = ca = cr = 0.0
            nobs = neg = ncsv = 0
            prev = None
            for j in range(ws, we):
                add(arr[j])
        else:
            for j in range(prev_s, ws):
                rem(arr[j])
            for j in range(prev_e, we):
                add(arr[j])
        if nobs >= w:
            r = sx / nobs
            if ncsv >= nobs:
                r = prev
            elif neg == 0 and r < 0:
                r = 0.0
            elif neg == nobs and r > 0:
                r = 0.0
            out[i] = r
        prev_s, prev_e = ws, we
    return out


if __name__ == "__main__":
    s = [10.0, 11.2, 10.8, 12.5, 13.1, 12.9, 14.2, 13.7, 15.0, 15.6,
         15.2, 16.1, 15.8, 17.0, 16.5, 17.8, 18.2, 17.9, 19.1, 18.6]
    g = pd.Series(s).rolling(5).mean().tolist()
    got = roll_mean_pandas(s, 5)
    bad = [(i, repr(a), repr(b)) for i, (a, b) in enumerate(zip(g, got)) if repr(a) != repr(b)]
    print("golden case match:", not bad, bad[:3])

    random.seed(42)
    ok = True
    for trial in range(3000):
        n = random.randint(3, 80)
        w = random.randint(1, min(20, n))
        sv = [round(random.uniform(-1000, 1000), random.randint(0, 6)) for _ in range(n)]
        exp = pd.Series(sv).rolling(w).mean().tolist()
        gt = roll_mean_pandas(sv, w)
        for a, b in zip(exp, gt):
            a_nan = a is None or (isinstance(a, float) and math.isnan(a))
            if a_nan:
                if b is not None:
                    ok = False
                    print("NULL mismatch", trial, w)
                    break
                continue
            if repr(a) != repr(b):
                ok = False
                print("MISMATCH t", trial, "w", w, repr(a), repr(b), sv)
                break
        if not ok:
            break
    print("random match:", ok)
