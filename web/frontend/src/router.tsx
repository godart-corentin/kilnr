import { createRootRoute, createRoute, createRouter, Outlet } from '@tanstack/react-router'
import { BuildPage } from './routes/BuildPage'
import { BuildsPage } from './routes/BuildsPage'
import { LogPage } from './routes/LogPage'

function RootLayout() {
  return (
    <main className="shell">
      <div className="topbar">
        <a className="brand" href="/">
          <svg className="brand-mark" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
            <path d="M12 3 22 20H2L12 3Z" fill="currentColor" />
          </svg>
          <span>Kilnr</span>
        </a>
        <span className="muted">CI</span>
      </div>
      <Outlet />
    </main>
  )
}

const rootRoute = createRootRoute({ component: RootLayout })

const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/',
  component: BuildsPage,
})

const buildRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/build/$buildId',
  component: BuildRouteComponent,
})

function BuildRouteComponent() {
  const { buildId } = buildRoute.useParams()
  return <BuildPage buildId={buildId} />
}

const logRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/build/$buildId/logs/$job',
  component: LogRouteComponent,
})

function LogRouteComponent() {
  const { buildId, job } = logRoute.useParams()
  return <LogPage buildId={buildId} job={job} />
}

const routeTree = rootRoute.addChildren([indexRoute, buildRoute, logRoute])
export const router = createRouter({ routeTree })

declare module '@tanstack/react-router' {
  interface Register {
    router: typeof router
  }
}
