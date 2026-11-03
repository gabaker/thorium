import styled from 'styled-components';

/// The top header bar: count on the left, title centered, create button on the right
export const HeaderBar = styled.div`
  display: flex;
  justify-content: space-between;
`;

/// The info note under the header, held one rem clear of the omnibar below it
export const NoteRow = styled.div`
  margin-bottom: 1rem;
`;

/// Centered row holding the pipelines omnibar
export const OmnibarRow = styled.div`
  display: flex;
  justify-content: center;
`;
