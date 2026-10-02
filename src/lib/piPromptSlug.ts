const WINDOWS_RESERVED_BASENAME =
  /^(?:con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)/i;
// eslint-disable-next-line no-control-regex -- 目的就是剔除控制字符（Windows 非法文件名字符），非误用
const PORTABLE_FILENAME_FORBIDDEN = /[\u0000-\u001f\u007f-\u009f<>:"/\\|?*]/u;

export function isValidPiPromptTemplateSlug(value: string): boolean {
  return (
    value.length > 0 &&
    new TextEncoder().encode(value).byteLength <= 128 &&
    value !== "." &&
    value !== ".." &&
    !value.startsWith(".") &&
    !value.endsWith(".") &&
    !/\s/u.test(value) &&
    !PORTABLE_FILENAME_FORBIDDEN.test(value) &&
    !WINDOWS_RESERVED_BASENAME.test(value)
  );
}
