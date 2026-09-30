import { useTranslation } from "react-i18next";
import { DeepLinkImportRequest } from "../../lib/api/deeplink";

export function SkillConfirmation({
  request,
}: {
  request: DeepLinkImportRequest;
}) {
  const { t } = useTranslation();

  // 解析 owner/name，用于醒目展示"来自第三方仓库"。
  // repo 格式已在后端 parser 校验为 `owner/name`，此处 split 失败时回退到原始串。
  const [owner, name] = (request.repo || "").split("/");
  const willEnable = request.enabled === true;

  return (
    <div className="space-y-4">
      <h3 className="text-lg font-semibold">{t("deeplink.skill.title")}</h3>

      <div>
        <label className="block text-sm font-medium text-muted-foreground">
          {t("deeplink.skill.repo")}
        </label>
        <div className="mt-1 text-sm font-mono bg-muted/50 p-2 rounded border break-all">
          {request.repo}
        </div>
        {/* owner 单独高亮：用户最容易扫一眼就点导入，把"谁的仓库"拆出来
            放在视线落点上，比整串 owner/name 更难被忽略。 */}
        {owner && (
          <div className="mt-1 text-xs text-muted-foreground">
            {t("deeplink.skill.owner", { defaultValue: "仓库所有者" })}:{" "}
            <span className="font-mono font-semibold text-yellow-700 dark:text-yellow-500">
              {owner}
            </span>
            {name ? ` / ${name}` : ""}
          </div>
        )}
      </div>

      <div>
        <label className="block text-sm font-medium text-muted-foreground">
          {t("deeplink.skill.directory")}
        </label>
        <div className="mt-1 text-sm font-mono bg-muted/50 p-2 rounded border break-all">
          {request.directory || "—"}
        </div>
      </div>

      <div>
        <label className="block text-sm font-medium text-muted-foreground">
          {t("deeplink.skill.branch")}
        </label>
        <div className="mt-1 text-sm font-mono">{request.branch || "main"}</div>
      </div>

      {/* 启用状态徽章：与后端 `enabled.unwrap_or(false)` 严格对齐。
          只有链接显式携带 enabled=true 才显示"已启用"；否则显示"未启用"，
          让用户知道导入后还需要手动开启。 */}
      <div>
        <label className="block text-sm font-medium text-muted-foreground">
          {t("deeplink.skill.enabledState", { defaultValue: "导入后状态" })}
        </label>
        <span
          className={`mt-1 inline-flex items-center px-2 py-0.5 rounded-md text-xs font-medium ${
            willEnable
              ? "bg-green-100 dark:bg-green-900/30 text-green-700 dark:text-green-300"
              : "bg-gray-100 dark:bg-gray-800 text-gray-600 dark:text-gray-400"
          }`}
        >
          {willEnable
            ? t("deeplink.skill.enabledBadge", { defaultValue: "已启用" })
            : t("deeplink.skill.disabledBadge", {
                defaultValue: "未启用（需手动开启）",
              })}
        </span>
      </div>

      {/* 第三方仓库警示：无条件显示，不依赖 enabled。
          skill 内容会从 GitHub 拉取并落盘到 ~/.claude/skills/，其中可能包含
          会被 Claude Code 加载执行的指令文件。用户必须知道来源不是官方。 */}
      <div className="text-yellow-700 dark:text-yellow-500 text-sm bg-yellow-50 dark:bg-yellow-950/30 p-3 rounded border border-yellow-400/40 space-y-1">
        <p className="flex items-start gap-2">
          <span aria-hidden="true">⚠️</span>
          <span>
            {t("deeplink.skill.thirdPartyWarning", {
              defaultValue:
                "此 Skill 来自第三方 GitHub 仓库，非 CC Switch 官方提供。导入后其内容会被 Claude Code 加载，可能影响 AI 行为。请确认你信任该仓库所有者。",
            })}
          </span>
        </p>
      </div>

      <div className="text-blue-600 dark:text-blue-400 text-sm bg-blue-50 dark:bg-blue-950/30 p-3 rounded border border-blue-200 dark:border-blue-800">
        <p>ℹ️ {t("deeplink.skill.hint")}</p>
        <p className="mt-1">{t("deeplink.skill.hintDetail")}</p>
      </div>
    </div>
  );
}
