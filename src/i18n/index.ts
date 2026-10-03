import i18n from "i18next";
import { initReactI18next } from "react-i18next";

import en from "./locales/en.json";
import zh from "./locales/zh.json";

type Language = "zh" | "zh-TW" | "en" | "ja";

/** 需要动态 import 语言包的语言。其余（zh/en）静态引入，见下方说明。 */
type LazyLanguage = "ja" | "zh-TW";

const DEFAULT_LANGUAGE: Language = "zh";

const getInitialLanguage = (): Language => {
  if (typeof window !== "undefined") {
    try {
      const stored = window.localStorage.getItem("language");
      if (
        stored === "zh" ||
        stored === "zh-TW" ||
        stored === "en" ||
        stored === "ja"
      ) {
        return stored;
      }
    } catch (error) {
      console.warn("[i18n] Failed to read stored language preference", error);
    }
  }

  const navigatorLang =
    typeof navigator !== "undefined"
      ? (navigator.language?.toLowerCase() ??
        navigator.languages?.[0]?.toLowerCase())
      : undefined;

  if (navigatorLang === "zh") {
    return "zh";
  }

  if (
    navigatorLang?.startsWith("zh-tw") ||
    navigatorLang?.startsWith("zh-hk") ||
    navigatorLang?.startsWith("zh-mo") ||
    navigatorLang?.startsWith("zh-hant")
  ) {
    return "zh-TW";
  }

  if (navigatorLang?.startsWith("zh")) {
    return "zh";
  }

  if (navigatorLang?.startsWith("ja")) {
    return "ja";
  }

  if (navigatorLang?.startsWith("en")) {
    return "en";
  }

  return DEFAULT_LANGUAGE;
};

/**
 * 语言包按需加载。
 *
 * 四份语言包合计 524 KB，此前全部静态 import 进主 chunk——但用户任何时刻只用其中
 * 一种，至少 3 份（279 KB）是永不加载的死重量，还拖慢 WebView 解析 4 MB bundle。
 *
 * 拆分策略是「同步保底 + 异步补齐」，不是全异步：
 * - `zh`（默认）与 `en`（fallbackLng）静态引入。i18n 的 init 必须同步可用——
 *   `main.tsx` 的配置加载失败提示直接调 `i18n.t()`，全异步会让首屏崩在白屏上。
 * - `ja` / `zh-TW` 动态 import，切换到这两种语言时才拉取对应 chunk。
 *
 * 代价：切到未加载语言时会先显示 fallback 英文，chunk 到达后 i18next 触发重渲染
 * 补正。桌面端语言切换是用户主动操作，这个瞬时无感。
 */
const loaders: Record<
  LazyLanguage,
  () => Promise<{ default: Record<string, unknown> }>
> = {
  ja: () => import("./locales/ja.json"),
  "zh-TW": () => import("./locales/zh-TW.json"),
};

/** 已加载的语言包，避免重复 import 同一 chunk。 */
const loaded = new Set<Language>(["zh", "en"]);

/** 语言包是否已在内存。供设置页判断"还需不需要拉"。 */
export const isLanguageLoaded = (lang: string): boolean =>
  loaded.has(lang as Language);

/**
 * 确保指定语言的语言包已就位；已加载则立即返回 true。
 *
 * 失败只告警不抛：单个语言包加载失败不该让应用白屏，退回 fallback 文本即可。
 */
export const ensureLanguageLoaded = async (lang: string): Promise<boolean> => {
  const loader = loaders[lang as LazyLanguage];
  if (!loader) {
    // zh / en 静态引入，恒可用
    return true;
  }
  if (loaded.has(lang as Language)) {
    return true;
  }
  try {
    const mod = await loader();
    i18n.addResourceBundle(
      lang,
      "translation",
      mod.default as Record<string, unknown>,
      true,
      true,
    );
    loaded.add(lang as Language);
    return true;
  } catch (error) {
    console.warn(`[i18n] Failed to load locale bundle: ${lang}`, error);
    return false;
  }
};

const initialLanguage = getInitialLanguage();

i18n.use(initReactI18next).init({
  resources: {
    en: { translation: en },
    zh: { translation: zh },
  },
  lng: initialLanguage,
  fallbackLng: "en", // 如果缺少中文翻译则退回英文

  interpolation: {
    escapeValue: false, // React 已经默认转义
  },

  // 开发模式下显示调试信息
  debug: false,
});

// 初始语言若为 ja / zh-TW，补加载对应包。放在 init 之后，是为了让主 chunk 先
// 解析渲染，语言包到达后由 i18next 触发重渲染。
if (loaded.has(initialLanguage)) {
  void i18n.changeLanguage(initialLanguage);
} else {
  void ensureLanguageLoaded(initialLanguage).then(() =>
    i18n.changeLanguage(initialLanguage),
  );
}

export default i18n;
