import type { Page } from "@playwright/test";

export type ExpectedHttpError = number | { status: number; pathname: string };

export function isExpectedRetainedViewMiss(text: string, location: string, pageUrl: string): boolean {
  const resource = URL.canParse(location) ? new URL(location) : undefined;
  return text.startsWith("Failed to load resource: the server responded with a status of 404 (") &&
    resource?.origin === new URL(pageUrl).origin &&
    /^\/api\/v1\/sessions\/[^/]+$/u.test(resource.pathname) &&
    (resource.search === "" ||
      (resource.searchParams.size === 1 && resource.searchParams.get("presentation") === "compact"));
}

export function isExpectedHttpError(text: string, location: string, pageUrl: string, expected: ExpectedHttpError[]): boolean {
  const status = text.match(/^Failed to load resource: the server responded with a status of (\d+) \(/u)?.[1];
  if (status == null) return false;
  const resource = URL.canParse(location) ? new URL(location) : undefined;
  return expected.some((error) => typeof error === "number" ? error === Number(status)
    : error.status === Number(status) && resource?.origin === new URL(pageUrl).origin && resource.pathname === error.pathname);
}

export function collectBrowserErrors(page: Page, expectedHttpErrors: ExpectedHttpError[] = []): string[] {
  const errors: string[] = [];
  page.on("console", (message) => {
    if (message.type() !== "error") return;
    const text = message.text();
    // The first retained-view probe may miss before directory discovery. Its
    // display projection does not change that; cwd/owner/resource errors do.
    if (isExpectedRetainedViewMiss(text, message.location().url, page.url())) return;
    if (isExpectedHttpError(text, message.location().url, page.url(), expectedHttpErrors)) return;
    errors.push(text);
  });
  page.on("pageerror", (error) => errors.push(error.message));
  return errors;
}
