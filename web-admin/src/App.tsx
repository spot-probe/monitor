import { useCallback, useEffect, useState } from "react"
import { ExternalLink, LogOut, Moon, Sun, UserRound } from "lucide-react"
import { Toaster } from "sonner"

import { ADMIN_ITEMS, ADMIN_SECTIONS, Admin } from "@/components/Admin"
import { Login } from "@/components/Login"
import { Button } from "@/components/ui/button"
import { Skeleton } from "@/components/ui/skeleton"
import { api, provisioningSite, useNodes } from "@/lib/api"

type Me = { authed: boolean; github: boolean; site_name: string; public_page: boolean; site: string; can_provision: boolean }

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

export default function App() {
  const [path, go] = usePath()
  const [dark, toggleTheme] = useTheme()
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

  return (
    <div className="min-h-svh">
      <header className="sticky top-0 z-10 border-b bg-background/80 backdrop-blur">
        <div className="mx-auto flex max-w-7xl items-center gap-3 px-4 py-2.5">
          {/* A breadcrumb over a page title, where the site name alone used to be: the
              header said which product this was but never where in it you were. The site
              name is still the first crumb -- and the way back to the status page -- while
              the sidebar's brand block takes it over visually at md and up. */}
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
              <a href="/" className="transition-colors hover:text-foreground">
                {me.site_name || "Monitor"}
              </a>
              <span aria-hidden>/</span>
              <span>{pageGroup || "后台"}</span>
            </div>
            <h1 className="truncate text-base font-semibold tracking-tight md:text-xl">{pageTitle || "后台"}</h1>
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
          {/* One admin, reached by password or by GitHub, so there is no profile page to
              link to and no menu worth opening. The identity and the way out are shown
              side by side instead. */}
          <span className="hidden items-center gap-2 rounded-full border py-1 pr-2.5 pl-1 sm:flex">
            <span className="grid size-6 place-items-center rounded-full bg-tag text-tag-foreground" aria-hidden>
              <UserRound className="size-3.5" />
            </span>
            <span className="text-xs text-muted-foreground">管理员</span>
          </span>
          <Button variant="ghost" size="icon" onClick={signOut} title="退出登录">
            <LogOut />
          </Button>
        </div>
      </header>

      <main className="mx-auto max-w-7xl space-y-5 px-4 py-6">
        {error && <p className="text-sm text-danger-fg">{error}</p>}
        {!nodes ? (
          <Skeleton className="h-64" />
        ) : (
          <Admin
            path={path}
            go={go}
            nodes={sorted}
            refresh={refresh}
            // The hub's own public URL rather than this browser's address: the
            // panel is frequently reached over a loopback port behind a proxy,
            // while the install command and OAuth callback need the real one.
            site={me.site || location.origin}
            siteName={me.site_name || "Monitor"}
            canProvision={me.can_provision && !!provisioningSite(location.origin) && !!provisioningSite(me.site || location.origin)}
          />
        )}
      </main>

      <Toaster position="top-center" theme={dark ? "dark" : "light"} />
    </div>
  )
}
