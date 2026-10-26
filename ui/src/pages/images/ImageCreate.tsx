import { FC } from 'react';
import { useNavigate } from 'react-router-dom';

// project imports
import { Actions, BannerSlot, PageHead, PageTitle, Section } from '@components/agentic/shared';
import Page from '@components/pages/Page';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Button, ButtonVariant } from '@components/shared/buttons';

/** Explain that images are no longer created and point at tools, which replace them. */
const ImageCreate: FC = () => {
  const navigate = useNavigate();
  return (
    <Page title="Create Image · Thorium">
      <PageHead>
        <PageTitle>Create image</PageTitle>
      </PageHead>
      <BannerSlot>
        <AlertBanner severity={Severity.Info}>
          <span data-testid="image-gone">
            Images can no longer be created or updated: the API answers <code>410 Gone</code>. Create a tool instead.
          </span>
        </AlertBanner>
      </BannerSlot>
      <Section>
        <h2>Create a tool instead</h2>
        <p style={{ margin: 0 }}>
          A tool holds what an image used to: its environment, scheduling, and how each job runs. Publishing a spec creates an immutable
          version that pipelines, agents, and toolsets can follow at latest or pin. Existing images can still be viewed and deleted on the
          Images page.
        </p>
        <Actions style={{ marginTop: '0.75rem' }} data-testid="image-gone-actions">
          <Button variant={ButtonVariant.Ok} data-testid="goto-create-tool" onClick={() => void navigate('/tools/create')}>
            Create a tool
          </Button>
          <Button variant={ButtonVariant.Secondary} onClick={() => void navigate('/images')}>
            Back to images
          </Button>
        </Actions>
      </Section>
    </Page>
  );
};

export default ImageCreate;
