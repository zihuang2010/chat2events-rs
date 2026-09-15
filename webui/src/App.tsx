/**
 * 应用装配：装载数据、组装上下文、渲染当前视图。
 * 加载、失败、空三态都在这里统一兜住，视图本身只处理「有数据但筛不出来」那一种空。
 */

import { Navigate, Route, Routes, useLocation } from "react-router-dom";
import { lazy, Suspense } from "react";
import { useDataset } from "@/api/queries";
import { AppShell } from "@/components/layout/AppShell";
import { ErrorState, PageSkeleton } from "@/components/states";
import { useAnalytics } from "@/features/filters/useAnalytics";
import { useFilters } from "@/features/filters/useFilters";
import { Workbench } from "@/components/layout/Workbench";
const OverviewPage = lazy(() =>
  import("@/features/overview/OverviewPage").then((module) => ({ default: module.OverviewPage })),
);
const RoomsPage = lazy(() =>
  import("@/features/rooms/RoomsPage").then((module) => ({ default: module.RoomsPage })),
);
const EventsPage = lazy(() =>
  import("@/features/events/EventsPage").then((module) => ({ default: module.EventsPage })),
);
const AgentsPage = lazy(() =>
  import("@/features/agents/AgentsPage").then((module) => ({ default: module.AgentsPage })),
);
const DetailPage = lazy(() =>
  import("@/features/detail/DetailPage").then((module) => ({ default: module.DetailPage })),
);

export default function App() {
  const query = useDataset();
  const api = useFilters();
  const { search } = useLocation();

  if (query.isPending) {
    return (
      <AppShell>
        <PageSkeleton />
      </AppShell>
    );
  }

  if (query.isError) {
    return (
      <AppShell>
        <div className="c2e-page">
          <ErrorState error={query.error} onRetry={() => void query.refetch()} />
        </div>
      </AppShell>
    );
  }

  return <Loaded dataset={query.data} api={api} search={search} />;
}

function Loaded({
  dataset,
  api,
  search,
}: {
  dataset: NonNullable<ReturnType<typeof useDataset>["data"]>;
  api: ReturnType<typeof useFilters>;
  search: string;
}) {
  const analytics = useAnalytics(dataset, api.filters);
  return (
    <AppShell asOf={dataset.meta.days.at(-1)}>
      <Workbench>
        <Suspense fallback={<PageSkeleton />}>
          <Routes>
            <Route path="/" element={<Navigate to={{ pathname: "/overview", search }} replace />} />
            <Route path="/overview" element={<OverviewPage analytics={analytics} api={api} />} />
            <Route path="/rooms" element={<RoomsPage analytics={analytics} api={api} />} />
            <Route path="/events" element={<EventsPage analytics={analytics} api={api} />} />
            <Route path="/agents" element={<AgentsPage analytics={analytics} api={api} />} />
            <Route path="/detail" element={<DetailPage analytics={analytics} api={api} />} />
            <Route path="*" element={<Navigate to={{ pathname: "/overview", search }} replace />} />
          </Routes>
        </Suspense>
      </Workbench>
    </AppShell>
  );
}
