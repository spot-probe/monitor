import { useCallback, useEffect, useState } from "react"
import { ChevronRight, ExternalLink, KeyRound, LogOut, Moon, PanelLeftClose, PanelLeftOpen, Sun, UserRound } from "lucide-react"
import { Toaster } from "sonner"

import { ADMIN_ITEMS, ADMIN_SECTIONS, Admin } from "@/components/Admin"
import { Login } from "@/components/Login"
import { Button } from "@/components/ui/button"
import { Skeleton } from "@/components/ui/skeleton"
import { api, provisioningSite, useNodes } from "@/lib/api"

type Me = { authed: boolean; github: boolean; site_name: string; public_page: boolean; site: string; can_provision: boolean; login: string; version: string }

// `/admin` alone is not a page; it is normalised to the first section so that a
// bookmark and the OAuth redirect both resolve to a real route.
function normalise(p: string) {
  return p === "/admin" || p === "/admin/" ? "/admin/nodes" : p.replace(/\/$/, "") || "/admin/nodes"
}

function usePath() {
  const [path, setPath] = useState(() => {
    const start = normalise(location.pathname)
    if (start !== location.pathname) history.replaceState({}, "", start + location.search)
    return start
  })
  useEffect(() => {
    const sync = () => setPath(normalise(location.pathname))
    addEventListener("popstate", sync)
    return () => removeEventListener("popstate", sync)
  }, [])
  return [
    path,
    useCallback((next: string) => {
      const to = normalise(next)
      history.pushState({}, "", to)
      setPath(to)
    }, []),
  ] as const
}

function useTheme() {
  const [dark, setDark] = useState(() => {
    const saved = localStorage.getItem("theme")
    return saved ? saved === "dark" : matchMedia("(prefers-color-scheme: dark)").matches
  })
  useEffect(() => {
    document.documentElement.classList.toggle("dark", dark)
    localStorage.setItem("theme", dark ? "dark" : "light")
  }, [dark])
  return [dark, () => setDark((d) => !d)] as const
}

// Only meaningful at md and up, where the nav is a column; below that it is a
// horizontal scroller with nothing to collapse. Remembered, because a preference
// that resets on every page load is not a preference.
function useNavOpen() {
  const [open, setOpen] = useState(() => localStorage.getItem("nav") !== "closed")
  useEffect(() => {
    localStorage.setItem("nav", open ? "open" : "closed")
  }, [open])
  return [open, () => setOpen((v) => !v)] as const
}

export default function App() {
  const [path, go] = usePath()
  const [dark, toggleTheme] = useTheme()
  const [navOpen, toggleNav] = useNavOpen()
  const [me, setMe] = useState<Me | null>(null)
  const [meError, setMeError] = useState("")
  const { nodes, admin, error, refresh } = useNodes()

  const loadMe = useCallback(() => {
    // `|| "..."` because an empty message reads as no error: api() falls back to
    // res.statusText, which HTTP/2 and HTTP/3 removed, so a bodiless 502 from a
    // proxy arrives as "". The check below would then take the loading branch and
    // the retry button would never render.
    return api<Me>("/me")
      .then((next) => { setMe(next); setMeError("") })
      .catch((e: Error) => setMeError(e.message || "网络错误"))
  }, [])
  useEffect(() => {
    loadMe()
  }, [loadMe])

  // Every frame declares its audience. The hub closes the stream when the session
  // behind it is revoked -- signed out from another device, a password change, a
  // restore -- and the reconnect returns as anonymous: the public list, with
  // private nodes absent and every admin field empty, rendered inside a panel that
  // still appears signed in. `authed` is read only at mount and after signing in,
  // so nothing else detects this. /api/me already handles signing out.
  useEffect(() => {
    if (me?.authed && admin === false) loadMe()
  }, [admin, me?.authed, loadMe])

  // Only while there is nothing else to show. Login's onDone reloads /me, so a
  // transient failure in the second after signing in would otherwise replace the
  // entire signed-in panel with a full-page error while the node list streamed
  // normally.
  if (!me) return (
    <div className="grid min-h-svh place-items-center p-6 text-sm text-muted-foreground">
      {meError ? <div className="space-y-3 text-center"><p role="alert">加载失败：{meError}</p><Button onClick={loadMe}>重试</Button></div> : "加载中…"}
    </div>
  )

  if (!me.authed) {
    return (
      <>
        <Login github={me.github} onDone={() => { loadMe(); refresh(); go("/admin/nodes") }} />
        <Toaster position="top-center" theme={dark ? "dark" : "light"} />
      </>
    )
  }

  const sorted = [...(nodes ?? [])].sort((a, b) => a.sort - b.sort || a.id - b.id)

  async function signOut() {
    await api("/auth/logout", { method: "POST" }).catch(() => {})
    location.href = "/"
  }

  // The page's own name and the group it belongs to, both from the one table the
  // sidebar renders, so a route can never be titled differently from its nav entry.
  const pageTitle = ADMIN_ITEMS.find((item) => item.path === path)?.label ?? ""
  const pageGroup = ADMIN_SECTIONS.find((section) => section.items.some((item) => item.path === path))?.group ?? ""

  // The provisioning hint has to stay put while the content scrolls, so it belongs to
  // the shell's band rather than to the page -- and its condition is already the shell's
  // (canProvision below is the same expression).
  const canProvision = me.can_provision && !!provisioningSite(location.origin) && !!provisioningSite(me.site || location.origin)
  const hint = path === "/admin/nodes" && !canProvision ? "请通过 HTTPS 域名访问面板后添加或安装节点。" : ""

  return (
    // Two panes. The brand and the nav are a full-height column whose header sits on the
    // same line as the page tools; the brand used to sit inside the nav's scroll area,
    // which left the top-left corner empty and put two "Nvidia"s a few pixels apart.
    <div className="flex min-h-svh flex-col md:h-svh md:min-h-0 md:flex-row md:overflow-hidden">
      <aside
        className={`border-b bg-card transition-[width] duration-200 ease-in-out md:flex md:shrink-0 md:flex-col md:border-r md:border-b-0 ${
          navOpen ? "md:w-60" : "md:w-16"
        }`}
      >
        <div className="flex h-14 items-center gap-2 px-4 md:h-[60px]">
          {/* The same mark the browser tab carries, so the panel and its tab are one
              product rather than two that happen to share a name. */}
          {/* The logo and the text sit on one baseline -- an image's baseline is its own
              bottom edge, so this puts the letters' feet on the logo's feet rather than
              centring two boxes of different heights, which never lines them up. The row
              keeps its fixed height and centres this wrapper, so the bar stays put. */}
          <span className="flex min-w-0 items-baseline gap-2">
            {/* The mark's own viewBox carries 12.5% empty space below the bars (bars span
                y 2..14 of a 16-unit box), so its box bottom sits 3.5px below the bars at
                28px. Baseline alignment would then put the text 3.5px under the bars --
                which is what "文字在 logo 下部" was. Cancelling that padding makes the
                image's effective bottom the bars' bottom, and the text lands on it. */}
            <img src="/favicon.svg" alt="" className="-mb-[3.5px] size-7 shrink-0" />
            {/* Name then version, as separate spans so the name can truncate without
                taking the version with it. Collapsed, both go together. */}
            <span className={`flex min-w-0 items-baseline gap-1.5 ${navOpen ? "" : "md:hidden"}`}>
              {/* `truncate` means overflow:hidden, and a box that clips reports its bottom
                  edge as its baseline -- which is the 6px that kept the name off the logo's
                  feet. Truncation is nested one level down instead, so the flex item keeps
                  the text's own baseline. */}
              <span className="min-w-0">
                <span className="block truncate text-base font-semibold tracking-tight">{me.site_name || "Monitor"}</span>
              </span>
              {me.version && (
                <span className="shrink-0 text-[10px] font-normal text-muted-foreground" title={`monitor-hub v${me.version}`}>
                  v{me.version}
                </span>
              )}
            </span>
          </span>
        </div>
        {/* Below md this is a horizontal scroller, and group headings would be words
            wedged in among the buttons. */}
        <nav className="flex gap-1 overflow-x-auto px-2 pb-2 md:min-h-0 md:flex-1 md:flex-col md:gap-5 md:overflow-y-auto md:px-2.5 md:pt-6 md:pb-4">
          {ADMIN_SECTIONS.map((section) => (
            <div key={section.group} className="flex gap-1 md:flex-col md:gap-1">
              <span className={`hidden px-3 pb-2 text-[11px] font-medium tracking-wide text-muted-foreground md:block ${navOpen ? "" : "md:hidden"}`}>
                {section.group}
              </span>
              {section.items.map(({ path: to, label, icon: Icon }) => {
                const active = path === to
                return (
                  <button
                    key={to}
                    onClick={() => go(to)}
                    aria-current={active ? "page" : undefined}
                    title={navOpen ? undefined : label}
                    className={`relative flex shrink-0 items-center gap-2 rounded-md px-3 py-2 text-sm transition-colors ${
                      navOpen ? "" : "md:justify-center"
                    } ${
                      active ? "bg-accent font-medium text-primary" : "text-muted-foreground hover:bg-muted hover:text-foreground"
                    }`}
                  >
                    {/* A bar on the leading edge: a tinted background alone sat too close to
                        the hover state to read as "you are here". */}
                    {active && <span className="absolute inset-y-1.5 left-0 w-[3px] rounded-full bg-primary" aria-hidden />}
                    <Icon className="size-4 shrink-0" />
                    {/* Collapsed, the icon is the label and the title attribute is the
                        tooltip -- the alternative is a tooltip primitive for one case. */}
                    <span className={navOpen ? "" : "md:hidden"}>{label}</span>
                  </button>
                )
              })}
            </div>
          ))}
        </nav>
        {/* Account controls belong at the foot of the column rather than in the header:
            the header's right end is for the page's own actions (状态面板, 主题切换), and
            this is the one place in the layout that is about you and not the page. */}
        <div className="hidden border-t p-2 md:mt-auto md:block">
          {/* The whole footer is one card, and it is deliberately the one tinted surface
              in the sidebar: this is the account, and which door it came through is a
              security fact. Off-white on white had no edge at all -- the card dissolved
              into the column -- so it carries the accent fill and a primary-toned border,
              which is also what makes the key icon legible as a credential rather than
              decoration. All three are theme tokens, so dark mode follows. */}
          <div className={`flex items-center gap-2.5 rounded-lg border border-primary/20 bg-accent px-3 py-2.5 ${navOpen ? "" : "md:flex-col md:gap-1.5 md:px-0"}`}>
            <span className="grid size-7 shrink-0 place-items-center text-primary" aria-hidden>
              {me.login ? <UserRound className="size-4" /> : <KeyRound className="size-4" />}
            </span>
            {/* Collapsed, the icon is the identity and the title attribute is the label.
                lucide no longer ships brand marks, so there is no GitHub logo to use, and
                a hover popover would mean a new dependency for one label. */}
            {navOpen && (
              <span className="min-w-0 flex-1">
                <span className="block truncate text-xs font-semibold">{me.login || "应急密码"}</span>
                <span className="block truncate text-[10px] text-muted-foreground">
                  {me.login ? "GitHub OAuth" : "备用登录方式"}
                </span>
              </span>
            )}
            <Button
              variant="ghost"
              size="icon"
              onClick={signOut}
              title="退出登录"
              aria-label="退出登录"
              className="shrink-0 text-muted-foreground hover:bg-background hover:text-destructive"
            >
              <LogOut />
            </Button>
          </div>
        </div>
      </aside>

      {/* min-h-0 on both: a flex child will not shrink below its content without it, and
          the content area is the only part allowed to scroll. */}
      <div className="flex min-w-0 flex-1 flex-col md:min-h-0">
        {/* A surface of its own rather than the page's colour. With both the same, the
            title and the tools read as floating on the background instead of forming the
            bar they are. */}
        <header className="sticky top-0 z-10 border-b bg-card/95 backdrop-blur">
          <div className="flex h-14 items-center gap-3 px-4 md:h-[60px]">
            {/* One line, so the breadcrumb shares an axis with the tools at the far end.
                It used to be stacked over the page title, which put the tools on the
                centre of a two-line block and left nothing actually aligned.
                The site name is deliberately absent: the brand column beside this says
                it, and its first crumb is the way back to the status page instead. */}
            {/* The toggle lives at md and up only: below that the nav is a horizontal
                scroller with nothing to collapse. */}
            <button
              type="button"
              onClick={toggleNav}
              title={navOpen ? "收起侧栏" : "展开侧栏"}
              aria-label={navOpen ? "收起侧栏" : "展开侧栏"}
              aria-expanded={navOpen}
              className="hidden size-8 shrink-0 place-items-center rounded-md text-muted-foreground transition-colors hover:bg-muted hover:text-foreground md:grid"
            >
              {navOpen ? <PanelLeftClose className="size-4" /> : <PanelLeftOpen className="size-4" />}
            </button>
            {/* A real element rather than a `|` character: a pipe sits on the baseline and
                cannot be given a height. */}
            <span className="hidden h-3.5 w-px shrink-0 bg-border md:block" aria-hidden />
            <div className="flex min-w-0 flex-1 items-center gap-1.5 text-[13px] text-muted-foreground">
              {/* The panel's own root, not the status page: an `<a href="/">` here sent
                  anyone clicking the first crumb out of the panel entirely, and 状态面板
                  at the other end is already the way to the public page. */}
              <button
                type="button"
                onClick={() => go("/admin/nodes")}
                className="transition-colors hover:text-foreground"
              >
                后台
              </button>
              {/* A chevron rather than a slash: it is the conventional separator, and it
                  is a real element that can be sized, unlike a text pipe. */}
              <ChevronRight className="size-3.5 shrink-0 text-muted-foreground/60" aria-hidden />
              <span className="truncate font-semibold text-foreground">{pageGroup || "总览"}</span>
            </div>
            {/* The status page is a separate app, so this is a navigation. */}
            <Button variant="ghost" size="sm" asChild>
              <a href="/">
                <ExternalLink /> <span className="hidden sm:inline">状态面板</span>
              </a>
            </Button>
            <Button variant="ghost" size="icon" onClick={toggleTheme} title="切换主题">
              {dark ? <Sun /> : <Moon />}
            </Button>
            {/* Below md the sidebar has no footer to put these in -- the nav is a
                horizontal scroller there -- so the account controls stay here, and the
                two forms are never on screen at the same width. */}
            <span className="flex items-center gap-1.5 sm:gap-2 md:hidden">
              <span className="grid size-7 shrink-0 place-items-center rounded-md bg-tag text-tag-foreground" aria-hidden>
                {me.login ? <UserRound className="size-3.5" /> : <KeyRound className="size-3.5" />}
              </span>
              <span
                className="hidden max-w-24 truncate text-xs text-muted-foreground sm:inline"
                title={me.login ? `GitHub · ${me.login}` : "应急密码登入"}
              >
                {me.login || "应急密码"}
              </span>
            </span>
            <Button
              variant="ghost"
              size="icon"
              onClick={signOut}
              title="退出登录"
              aria-label="退出登录"
              className="text-muted-foreground hover:bg-destructive/10 hover:text-destructive md:hidden"
            >
              <LogOut />
            </Button>
          </div>
        </header>

        <main className="min-w-0 flex-1 space-y-5 p-4 md:min-h-0 md:overflow-y-auto md:p-6">
          {/* The page's heading lives with the page. In the bar it had to share a line
              with the breadcrumb, and the bar is what should stay one line. */}
          <div className="sticky -top-4 z-[1] -mx-4 -mt-4 border-b bg-background px-4 pt-4 pb-3 md:-top-6 md:-mx-6 md:-mt-6 md:px-6 md:pt-6">
            <h1 className="truncate text-xl font-semibold tracking-tight">{pageTitle || "后台"}</h1>
            {hint && <p className="mt-1.5 text-xs leading-relaxed text-muted-foreground">{hint}</p>}
          </div>
          {error && <p className="text-sm text-danger-fg">{error}</p>}
          {!nodes ? (
            <Skeleton className="h-64" />
          ) : (
            <Admin
              path={path}
              nodes={sorted}
              refresh={refresh}
              // The hub's own public URL rather than this browser's address: the
              // panel is frequently reached over a loopback port behind a proxy,
              // while the install command and OAuth callback need the real one.
              site={me.site || location.origin}
              canProvision={canProvision}
            />
          )}
        </main>

        <Toaster position="top-center" theme={dark ? "dark" : "light"} />
      </div>
    </div>
  )
}
