export function resolveProviderIcon(icon?: string): string | undefined {
  const normalizedIcon = icon?.trim();
  if (!normalizedIcon) return undefined;

  return normalizedIcon;
}
