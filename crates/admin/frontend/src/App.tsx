import { useEffect, useState } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { BrowserRouter, Navigate, Route, Routes } from "react-router";
import { UNAUTHORIZED_EVENT, getToken } from "./lib/api";
import TokenGate from "./components/TokenGate";
import Layout from "./components/Layout";
import Overview from "./pages/Overview";
import Builders from "./pages/Builders";
import PendingPromotion from "./pages/PendingPromotion";
import BuilderGroups from "./pages/BuilderGroups";
import BuilderGroupDetail from "./pages/BuilderGroupDetail";
import Demotions from "./pages/Demotions";
import Payloads from "./pages/Payloads";
import Settings from "./pages/Settings";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: 1,
      refetchOnWindowFocus: false,
    },
  },
});

export default function App() {
  const [authed, setAuthed] = useState(() => getToken() !== null);

  useEffect(() => {
    const onUnauthorized = () => {
      queryClient.clear();
      setAuthed(false);
    };
    window.addEventListener(UNAUTHORIZED_EVENT, onUnauthorized);
    return () => window.removeEventListener(UNAUTHORIZED_EVENT, onUnauthorized);
  }, []);

  return (
    <QueryClientProvider client={queryClient}>
      {authed ? (
        <BrowserRouter>
          <Routes>
            <Route element={<Layout />}>
              <Route path="/" element={<Overview />} />
              <Route path="/builders" element={<Builders />}>
                <Route index element={<Navigate to="pending" replace />} />
                <Route path="pending" element={<PendingPromotion />} />
                <Route path="groups" element={<BuilderGroups />} />
                <Route path="groups/:groupKey" element={<BuilderGroupDetail />} />
              </Route>
              <Route path="/demotions" element={<Demotions />} />
              <Route path="/payloads" element={<Payloads />} />
              <Route path="/settings" element={<Settings />} />
              <Route path="*" element={<Navigate to="/" replace />} />
            </Route>
          </Routes>
        </BrowserRouter>
      ) : (
        <TokenGate onAuthenticated={() => setAuthed(true)} />
      )}
    </QueryClientProvider>
  );
}
