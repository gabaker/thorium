import { FC, useState } from 'react';

// project imports
import { catalogBans } from '@components/agentic/models/format';
import { useBanNotifications } from '@components/agentic/models/notifications';
import { CredentialTable, EndpointTable, ProfileTable } from '@components/agentic/models/ModelRows';
import { useAgenticList } from '@components/agentic/hooks';
import { BannerSlot, PageHead, PageTitle, Section, TableScroll } from '@components/agentic/shared';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';
import Page from '@components/pages/Page';
import { Tabs } from '@components/shared/tabs';
import { AgenticKinds } from '@models/agentic';
import { ModelCredential, ModelEndpoint, ModelProfile } from '@models/modelconfigs';

/** The tabs of the models page. */
enum ModelTab {
  /** Provider connections */
  Endpoints = 'endpoints',
  /** Per-model call settings */
  Profiles = 'profiles',
  /** Authentication for endpoints */
  Credentials = 'credentials',
}

/** List model endpoints, profiles, and credentials with their bans and notifications (read only). */
const ModelBrowsing: FC = () => {
  const [tab, setTab] = useState<ModelTab>(ModelTab.Endpoints);
  const endpoints = useAgenticList<ModelEndpoint>(AgenticKinds.ModelEndpoint);
  const profiles = useAgenticList<ModelProfile>(AgenticKinds.ModelProfile);
  const credentials = useAgenticList<ModelCredential>(AgenticKinds.ModelCredential);
  // ban notifications are loaded once for the banned entities of every tab; an endpoint's
  // notifications also cover the bans on its catalog models
  const notes = useBanNotifications([
    ...endpoints.items.map((item) => ({
      id: item.id,
      bans: {
        ...item.metadata.ModelEndpoint.bans,
        ...Object.fromEntries(catalogBans(item.metadata.ModelEndpoint.catalog).map(({ ban }) => [ban.id, ban])),
      },
    })),
    ...profiles.items.map((item) => ({ id: item.id, bans: item.metadata.ModelProfile.bans })),
    ...credentials.items.map((item) => ({ id: item.id, bans: item.metadata.ModelCredential.bans })),
  ]);
  // a count is shown only once its list has loaded; a list that is still loading or failed has no count to show
  const countOf = (list: { items: unknown[]; loading: boolean; error: string }): number | undefined =>
    list.loading || list.error ? undefined : list.items.length;
  const loading = endpoints.loading || profiles.loading || credentials.loading;
  const error = endpoints.error || profiles.error || credentials.error || notes.error;
  return (
    <Page title="Models · Thorium">
      <PageHead>
        <PageTitle>Models</PageTitle>
      </PageHead>
      <BannerSlot>{error && <AlertBanner severity={Severity.Error}>{error}</AlertBanner>}</BannerSlot>
      <Tabs
        active={tab}
        onChange={setTab}
        tabs={[
          { key: ModelTab.Endpoints, label: 'Endpoints', count: countOf(endpoints) },
          { key: ModelTab.Profiles, label: 'Profiles', count: countOf(profiles) },
          { key: ModelTab.Credentials, label: 'Credentials', count: countOf(credentials) },
        ]}
      />
      <Section>
        {/* the spinner takes the place of the tables until every list has loaded */}
        <LoadingSpinner loading={loading} />
        {!loading && (
          <TableScroll>
            {tab === ModelTab.Endpoints && (
              <EndpointTable
                items={endpoints.items}
                credentials={credentials.items}
                credentialsFailed={!!credentials.error}
                credentialsLoading={credentials.loading}
                notifications={notes.byId}
                loading={endpoints.loading || credentials.loading}
                failed={!!endpoints.error}
              />
            )}
            {tab === ModelTab.Profiles && (
              <ProfileTable
                items={profiles.items}
                endpoints={endpoints.items}
                notifications={notes.byId}
                loading={profiles.loading || endpoints.loading}
                failed={!!profiles.error}
              />
            )}
            {tab === ModelTab.Credentials && (
              <CredentialTable
                items={credentials.items}
                notifications={notes.byId}
                loading={credentials.loading}
                failed={!!credentials.error}
              />
            )}
          </TableScroll>
        )}
      </Section>
    </Page>
  );
};

export default ModelBrowsing;
