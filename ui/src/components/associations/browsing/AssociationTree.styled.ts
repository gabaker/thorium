import styled from 'styled-components';
import { Popover } from 'react-bootstrap';

export const TreeContainer = styled.div`
  .tree button[role='treeitem'] {
    display: flex;
    background: transparent;
    border: none;
    width: 100%;
    padding: 0 0 2px 0;
  }

  .treeitem {
    width: 100%;
    text-align: left;
    color: var(--thorium-text);
    padding: 6px 10px;
    position: relative;
    border-radius: 8px;
    transition: background-color 0.2s ease, outline-color 0.2s ease;
    cursor: pointer;
    display: flex;
    align-items: center;
  }
  .treeitem:hover {
    background-color: rgb(0, 102, 255, 0.1);
    color: var(--thorium-text);
    border-color: black;
  }

  .tree button[role='treeitem']:focus {
    outline: none;
  }

  button:focus-visible .treeitem.focused,
  .treeitem.searchmatch.focused {
    outline: 2px solid black;
  }

  .treeitem.drop {
    border-color: var(--selected-color);
    background-color: #e1f1f8;
  }

  .treeitem.searchmatch {
    background-color: #e1f8ff;
  }

  .treeitem.folder:before {
    content: url(data:image/svg+xml;base64,PHN2ZyB2ZXJzaW9uPSIxLjEiIHhtbG5zPSJodHRwOi8vd3d3LnczLm9yZy8yMDAwL3N2ZyIgeG1sbnM6eGxpbms9Imh0dHA6Ly93d3cudzMub3JnLzE5OTkveGxpbmsiIHg9IjBweCIgeT0iMHB4IiB2aWV3Qm94PSIwIDAgMTYgMTYiIGVuYWJsZS1iYWNrZ3JvdW5kPSJuZXcgMCAwIDE2IDE2IiB4bWw6c3BhY2U9InByZXNlcnZlIj48Zz48Zz48cGF0aCBmaWxsLXJ1bGU9ImV2ZW5vZGQiIGNsaXAtcnVsZT0iZXZlbm9kZCIgZD0iTTQuNjQ2IDEuNjQ2YS41LjUgMCAwIDEgLjcwOCAwbDYgNmEuNS41IDAgMCAxIDAgLjcwOGwtNiA2YS41LjUgMCAwIDEtLjcwOC0uNzA4TDEwLjI5MyA4IDQuNjQ2IDIuMzU0YS41LjUgMCAwIDEgMC0uNzA4eiIgY2xhc3M9InJjdC10cmVlLWl0ZW0tYXJyb3ctcGF0aCI+PC9wYXRoPjwvZz48L2c+PC9zdmc+);
    background-color: transparent;
    width: 10px;
    display: inline-block;
    z-index: 1;
    margin-right: 4px;
    transition: transform 0.1s ease-in-out;
  }

  .treeitem.folder.expanded:before {
    transform: rotate(90deg);
  }

  .treeitem:not(.folder) {
    padding-left: 24px;
  }

  .treeitem.selected:after {
    content: ' ';
    position: absolute;
    top: 5px;
    left: -2px;
    height: 24px;
    width: 4px;
    background-color: #0366d6;
    border-radius: 99px;
  }

  .treeitem.duplicate-highlight {
    outline: 2px dashed #e8a838;
    outline-offset: -2px;
    background-color: rgba(232, 168, 56, 0.12);
  }

  .treeitem.duplicate-highlight:hover {
    background-color: rgba(232, 168, 56, 0.2);
  }

  .outeritem {
    display: flex;
    align-items: center;
    gap: 2px;
  }
  .outeritem button:not([role='treeitem']) {
    padding: 2px 4px;
    height: 80%;
  }

  .node-type-icon {
    display: inline-block;
    width: 16px;
    height: 16px;
    margin-right: 12px;
    flex-shrink: 0;
  }

  .node-type-icon img {
    width: 100%;
    height: 100%;
  }

  .duplicate-indicator {
    display: inline-block;
    font-size: 0.6rem;
    padding: 0 4px;
    margin-left: 4px;
    border-radius: 3px;
    background-color: rgba(232, 168, 56, 0.25);
    color: #b07d1a;
    vertical-align: middle;
  }
`;

export const PreviewPopover = styled(Popover)`
  --bs-popover-max-width: 360px;
  --bs-popover-bg: var(--thorium-secondary-panel-bg);
  --bs-popover-border-color: var(--thorium-panel-border);
  --bs-popover-body-color: var(--thorium-text);
  --bs-popover-arrow-border: var(--thorium-panel-border);
  overflow: hidden;

  .popover-body {
    padding: 10px 14px;
    font-size: 0.82rem;
    overflow: hidden;
  }

  .preview-type {
    font-weight: 600;
    margin-bottom: 4px;
    color: var(--thorium-text);
  }

  .preview-field {
    margin-bottom: 2px;
    color: var(--thorium-secondary-text, var(--thorium-text));
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .preview-field strong {
    color: var(--thorium-text);
  }

  .preview-tags {
    display: flex;
    flex-wrap: wrap;
    gap: 3px;
    margin-top: 6px;
    overflow: hidden;
    max-width: 100%;
  }

  .preview-tag {
    font-size: 0.7rem;
    padding: 1px 6px;
    border-radius: 4px;
    background-color: rgba(66, 125, 140, 0.15);
    color: #427d8c;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 100%;
  }

  .preview-duplicate-warn {
    margin-top: 6px;
    padding: 3px 6px;
    font-size: 0.7rem;
    border-radius: 4px;
    background-color: rgba(232, 168, 56, 0.15);
    color: #b07d1a;
  }
`;
