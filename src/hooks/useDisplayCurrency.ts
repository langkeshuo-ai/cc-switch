import {
  DEFAULT_DISPLAY_CURRENCY,
  currencySymbol,
  type DisplayCurrency,
} from "@/components/usage/format";

/**
 * R3：读计价展示货币偏好（设备级，缺省 USD）。
 *
 * 返回符号字符串——调用方把它传给 `fmtUsd(value, digits, fallback, symbol)`。
 * 只影响渲染，不改存储与计算。
 *
 * ⚠️ 刻意不读 react-query 的 settings：用量页的多个图表组件会被**没有
 * QueryClientProvider 包裹**的测试直接渲染（如 UsageTrendChart.render.test），
 * 走 useSettingsQuery 会让它们全挂「No QueryClient set」。settings 保存时
 * 会镜像一份到 localStorage（见 useSettings 的 autoSave/saveSettings），
 * 与 language 的持久化方式一致——localStorage 是同步、零依赖、永远可读。
 */
const STORAGE_KEY = "displayCurrency";

export function readDisplayCurrency(): DisplayCurrency {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    return raw === "CNY" ? "CNY" : DEFAULT_DISPLAY_CURRENCY;
  } catch {
    return DEFAULT_DISPLAY_CURRENCY;
  }
}

export function useDisplayCurrency(): {
  currency: DisplayCurrency;
  symbol: string;
} {
  const currency = readDisplayCurrency();
  return { currency, symbol: currencySymbol(currency) };
}

/** 保存 settings 时镜像到 localStorage（由 useSettings 的保存路径调用）。 */
export function persistDisplayCurrency(
  value: DisplayCurrency | undefined,
): void {
  try {
    if (value) {
      window.localStorage.setItem(STORAGE_KEY, value);
    } else {
      window.localStorage.removeItem(STORAGE_KEY);
    }
  } catch {
    /* localStorage 不可用（隐私模式）时静默：货币回落缺省，不影响功能 */
  }
}
