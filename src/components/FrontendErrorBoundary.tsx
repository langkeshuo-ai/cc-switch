import React from "react";
import { RefreshCw, TriangleAlert } from "lucide-react";
// 用 `useTranslation` 而不是直接 import `@/i18n`：后者会在模块加载时执行
// i18next 初始化，测试里一旦 mock 掉 `react-i18next`（未导出
// `initReactI18next`）就会让所有引用本文件的组件树一起挂掉。
import { useTranslation } from "react-i18next";
import { reportFrontendError } from "@/lib/frontendLogger";
import { Button } from "@/components/ui/button";

interface FrontendErrorBoundaryProps extends React.PropsWithChildren {
  /**
   * `app`（默认）：全屏兜底，用于 main.tsx 的根容器。
   * `panel`：面板内紧凑兜底 + 重试按钮，单个面板崩溃时只降级该面板。
   */
  variant?: "app" | "panel";
  /** 面板名，仅用于诊断日志定位 */
  panelName?: string;
}

interface FrontendErrorBoundaryState {
  hasError: boolean;
}

export class FrontendErrorBoundary extends React.Component<
  FrontendErrorBoundaryProps,
  FrontendErrorBoundaryState
> {
  state: FrontendErrorBoundaryState = { hasError: false };

  static getDerivedStateFromError(): FrontendErrorBoundaryState {
    return { hasError: true };
  }

  componentDidCatch(error: Error, info: React.ErrorInfo): void {
    reportFrontendError(
      this.props.variant === "panel"
        ? "react.error_boundary.panel"
        : "react.error_boundary",
      error,
      info.componentStack ?? undefined,
    );
  }

  private retry = (): void => {
    this.setState({ hasError: false });
  };

  render(): React.ReactNode {
    if (!this.state.hasError) {
      return this.props.children;
    }

    if (this.props.variant === "panel") {
      return <PanelCrashFallback onRetry={this.retry} />;
    }

    return <AppCrashFallback />;
  }
}

function PanelCrashFallback({
  onRetry,
}: {
  onRetry: () => void;
}): React.ReactElement {
  const { t } = useTranslation();

  return (
    <section
      role="alert"
      className="flex flex-col items-start gap-3 rounded-lg border border-border bg-card p-5 text-card-foreground"
    >
      <div className="flex items-start gap-3">
        <TriangleAlert className="mt-0.5 size-4 shrink-0 text-destructive" />
        <div className="space-y-1.5">
          <h3 className="text-sm font-semibold">
            {t("errors.panelCrashTitle", { defaultValue: "此面板遇到了问题" })}
          </h3>
          <p className="text-sm leading-6 text-muted-foreground">
            {t("errors.panelCrashMessage", {
              defaultValue:
                "其余功能不受影响。已尝试将错误信息写入应用诊断日志，可重试加载此面板。",
            })}
          </p>
        </div>
      </div>
      <Button variant="outline" size="sm" onClick={onRetry}>
        <RefreshCw className="mr-2 size-4" />
        {t("errors.retryPanel", { defaultValue: "重试" })}
      </Button>
    </section>
  );
}

function AppCrashFallback(): React.ReactElement {
  const { t } = useTranslation();

  return (
    <main className="flex min-h-screen items-center justify-center bg-background p-6 text-foreground">
      <section
        role="alert"
        className="w-full max-w-md space-y-5 rounded-lg border border-border bg-card p-6 shadow-sm"
      >
        <div className="flex items-start gap-3">
          <TriangleAlert className="mt-0.5 size-5 shrink-0 text-destructive" />
          <div className="space-y-1.5">
            <h1 className="text-base font-semibold">
              {t("errors.frontendCrashTitle", {
                defaultValue: "界面遇到了问题",
              })}
            </h1>
            <p className="text-sm leading-6 text-muted-foreground">
              {t("errors.frontendCrashMessage", {
                defaultValue:
                  "已尝试将错误信息写入应用诊断日志。请重新加载界面；如果问题持续，请在提交 Issue 时附上日志。",
              })}
            </p>
          </div>
        </div>
        <Button className="w-full" onClick={() => window.location.reload()}>
          <RefreshCw className="mr-2 size-4" />
          {t("errors.reloadInterface", { defaultValue: "重新加载界面" })}
        </Button>
      </section>
    </main>
  );
}
