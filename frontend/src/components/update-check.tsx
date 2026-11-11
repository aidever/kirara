import { useCallback, useEffect, useState } from "react";
import { ArrowUpCircle, ExternalLink, RefreshCw } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Notice } from "@/components/ui/notice";
import { api } from "@/lib/api";

/** 与后端 `UpdateStatus` 对应。 */
export type UpdateStatus = {
  enabled: boolean;
  current_version: string;
  latest_version: string | null;
  update_available: boolean;
  release_url: string;
  published_at: string | null;
  checked_at: string | null;
  last_error: string | null;
};

const STATUS_EVENT = "kirara:update-status";
const REFRESH_INTERVAL_MS = 30 * 60 * 1000;

function publish(status: UpdateStatus) {
  window.dispatchEvent(new CustomEvent<UpdateStatus>(STATUS_EVENT, { detail: status }));
}

function formatTime(value: string | null) {
  if (!value) return "";
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString("zh-CN", { hour12: false });
}

/** 读取新版本检查状态，并在设置页修改后同步更新。 */
export function useUpdateStatus() {
  const [status, setStatus] = useState<UpdateStatus | null>(null);
  useEffect(() => {
    let cancelled = false;
    const load = () => {
      api<UpdateStatus>("/api/update-check")
        .then((next) => { if (!cancelled) setStatus(next); })
        .catch(() => { /* 检查状态读取失败不影响其他页面 */ });
    };
    load();
    const timer = window.setInterval(load, REFRESH_INTERVAL_MS);
    const onStatus = (event: Event) => setStatus((event as CustomEvent<UpdateStatus>).detail);
    window.addEventListener(STATUS_EVENT, onStatus);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
      window.removeEventListener(STATUS_EVENT, onStatus);
    };
  }, []);
  return status;
}

/** 侧栏里的新版本提示；关闭检查或已是最新版时不显示。 */
export function UpdateNotice() {
  const status = useUpdateStatus();
  if (!status?.enabled || !status.update_available || !status.latest_version) return null;
  return (
    <a
      href={status.release_url}
      target="_blank"
      rel="noopener noreferrer"
      className="flex shrink-0 items-center gap-2 rounded-lg border border-border bg-card px-3 py-2.5 text-sm text-foreground transition hover:border-primary"
      title={`当前 ${status.current_version}，最新 ${status.latest_version}（新标签页打开发布页）`}
    >
      <ArrowUpCircle className="h-4 w-4 shrink-0 text-primary" aria-hidden="true" />
      <span className="min-w-0 flex-1 truncate">
        新版本 <span className="font-mono font-semibold">v{status.latest_version}</span> 可用
      </span>
      <ExternalLink className="h-3.5 w-3.5 shrink-0 text-muted" aria-hidden="true" />
    </a>
  );
}

/** 系统设置里的新版本检查开关。 */
export function UpdateCheckCard() {
  const [status, setStatus] = useState<UpdateStatus | null>(null);
  const [busy, setBusy] = useState<"toggle" | "check" | null>(null);
  const [error, setError] = useState("");

  useEffect(() => {
    api<UpdateStatus>("/api/update-check")
      .then(setStatus)
      .catch((reason: Error) => setError(reason.message || "读取失败"));
  }, []);

  const apply = useCallback((next: UpdateStatus) => {
    setStatus(next);
    publish(next);
  }, []);

  const toggle = async (enabled: boolean) => {
    setBusy("toggle");
    setError("");
    try {
      apply(await api<UpdateStatus>("/api/update-check", { method: "PUT", body: JSON.stringify({ enabled }) }));
    } catch (reason) {
      setError((reason as Error).message || "保存失败");
    } finally {
      setBusy(null);
    }
  };

  const checkNow = async () => {
    setBusy("check");
    setError("");
    try {
      apply(await api<UpdateStatus>("/api/update-check/run", { method: "POST" }));
    } catch (reason) {
      setError((reason as Error).message || "检查失败");
    } finally {
      setBusy(null);
    }
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle>新版本检查</CardTitle>
        <CardDescription>定时查询 GitHub 上的最新正式版，有新版本时在侧栏提醒。只提醒，不会自动下载或替换程序。</CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-3">
        <label className="flex cursor-pointer items-start gap-2.5">
          <input
            type="checkbox"
            className="mt-0.5 size-4 accent-primary"
            checked={status?.enabled ?? true}
            disabled={!status || busy !== null}
            onChange={(event) => void toggle(event.target.checked)}
          />
          <span>
            <span className="block text-sm font-semibold">自动检查新版本</span>
            <span className="block text-xs text-muted">每 6 小时访问一次 api.github.com（走全局代理），关闭后不再访问。</span>
          </span>
        </label>

        {status ? (
          <div className="flex flex-col gap-1 text-sm text-secondary-foreground">
            <span>当前版本 <span className="font-mono">v{status.current_version}</span></span>
            {status.latest_version ? (
              <span>
                最新正式版 <span className="font-mono">v{status.latest_version}</span>
                {status.update_available ? (
                  <>
                    {" · "}
                    <a href={status.release_url} target="_blank" rel="noopener noreferrer" className="font-medium text-primary hover:underline">
                      查看发布说明与下载
                    </a>
                  </>
                ) : " · 已是最新"}
              </span>
            ) : null}
            {status.checked_at ? <span className="text-xs text-muted">上次检查：{formatTime(status.checked_at)}</span> : null}
            {status.last_error ? <span className="text-xs text-destructive">上次检查失败：{status.last_error}</span> : null}
          </div>
        ) : null}

        <div>
          <Button type="button" variant="outline" size="sm" loading={busy === "check"} disabled={!status || busy !== null} onClick={() => void checkNow()}>
            {busy === "check" ? null : <RefreshCw aria-hidden="true" />}
            立即检查
          </Button>
        </div>
        {error ? <Notice tone="error" onDismiss={() => setError("")}>{error}</Notice> : null}
      </CardContent>
    </Card>
  );
}
