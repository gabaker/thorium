import * as THREE from 'three';
import type { NodeType, VisualState } from './types';

import FileSVG from '@assets/icons/file.svg?raw';
import FileSearchSVG from '@assets/icons/file-add.svg?raw';
import TagSVG from '@assets/icons/tag.svg?raw';
import TagSearchSVG from '@assets/icons/tag-add.svg?raw';
import RepoSVG from '@assets/icons/git.svg?raw';
import VendorSVG from '@assets/icons/vendor.svg?raw';
import VendorSearchSVG from '@assets/icons/vendor-add.svg?raw';
import DeviceSVG from '@assets/icons/device.svg?raw';
import CollectionSVG from '@assets/icons/collection.svg?raw';
import FileSystemSVG from '@assets/icons/filesystem.svg?raw';
import FolderSVG from '@assets/icons/folder.svg?raw';
import FolderSearchSVG from '@assets/icons/folder-add.svg?raw';
import OtherSVG from '@assets/icons/other.svg?raw';

const NODE_COLORS: Record<NodeType, Record<VisualState, string>> = {
  file: { basic: '#f1d592', growable: '#64cc66', initial: '#00998C' },
  repo: { basic: '#f03c2e', growable: '#64cc66', initial: '#00998C' },
  tag: { basic: '#427d8c', growable: '#64cc66', initial: '#00998C' },
  device: { basic: '#ed9624', growable: '#64cc66', initial: '#00998C' },
  vendor: { basic: '#8f30b8', growable: '#64cc66', initial: '#00998C' },
  collection: { basic: '#8f30b8', growable: '#64cc66', initial: '#00998C' },
  filesystem: { basic: '#8f30b8', growable: '#64cc66', initial: '#00998C' },
  folder: { basic: '#f1d592', growable: '#64cc66', initial: '#00998C' },
  other: { basic: '#cacfca', growable: '#64cc66', initial: '#00998C' },
};

export const getNodeColor = (nodeType: NodeType, visualState: VisualState): string => {
  return NODE_COLORS[nodeType]?.[visualState] ?? NODE_COLORS.other.basic;
};

const SVG_MAP: Record<NodeType, Record<VisualState, string>> = {
  file: {
    basic: FileSVG.replace('REPLACEME', 'f1d592'),
    growable: FileSearchSVG.replace('REPLACEME', '64cc66'),
    initial: FileSVG.replace('REPLACEME', '00998C'),
  },
  repo: {
    basic: RepoSVG.replace('REPLACEME', 'f03c2e'),
    growable: RepoSVG.replace('REPLACEME', '64cc66'),
    initial: RepoSVG.replace('REPLACEME', '00998C'),
  },
  tag: {
    basic: TagSVG.replace('REPLACEME', '427d8c'),
    growable: TagSearchSVG.replace('REPLACEME', '64cc66'),
    initial: TagSVG.replace('REPLACEME', '00998C'),
  },
  device: {
    basic: DeviceSVG.replace('REPLACEME', 'ed9624'),
    growable: DeviceSVG.replace('REPLACEME', '64cc66'),
    initial: DeviceSVG.replace('REPLACEME', '00998C'),
  },
  vendor: {
    basic: VendorSVG.replace('REPLACEME', '8f30b8'),
    growable: VendorSearchSVG.replace('REPLACEME', '64cc66'),
    initial: VendorSVG.replace('REPLACEME', '00998C'),
  },
  collection: {
    basic: CollectionSVG.replace('REPLACEME', '8f30b8'),
    growable: CollectionSVG.replace('REPLACEME', '64cc66'),
    initial: CollectionSVG.replace('REPLACEME', '00998C'),
  },
  filesystem: {
    basic: FileSystemSVG.replace('REPLACEME', '8f30b8'),
    growable: FileSystemSVG.replace('REPLACEME', '64cc66'),
    initial: FileSystemSVG.replace('REPLACEME', '00998C'),
  },
  folder: {
    basic: FolderSVG.replace('REPLACEME', 'f1d592'),
    growable: FolderSearchSVG.replace('REPLACEME', '64cc66'),
    initial: FolderSVG.replace('REPLACEME', '00998C'),
  },
  other: {
    basic: OtherSVG.replace('REPLACEME', 'cacfca'),
    growable: OtherSVG.replace('REPLACEME', '64cc66'),
    initial: OtherSVG.replace('REPLACEME', '00998C'),
  },
};

export const getNodeSvg = (nodeType: NodeType, visualState: VisualState): string => {
  return SVG_MAP[nodeType]?.[visualState] ?? SVG_MAP.other.basic;
};

const textureCache = new Map<string, THREE.Texture>();

export const svgToTexture = (svgString: string, size = 64): THREE.Texture => {
  const cacheKey = `${svgString.substring(0, 100)}_${size}`;
  const cached = textureCache.get(cacheKey);
  if (cached) return cached;

  const canvas = document.createElement('canvas');
  canvas.width = size;
  canvas.height = size;
  const ctx = canvas.getContext('2d')!;

  const img = new Image();
  const blob = new Blob([svgString], { type: 'image/svg+xml;charset=utf-8' });
  const url = URL.createObjectURL(blob);

  const texture = new THREE.Texture(canvas);
  img.onload = () => {
    ctx.drawImage(img, 0, 0, size, size);
    texture.needsUpdate = true;
    URL.revokeObjectURL(url);
  };
  img.src = url;

  textureCache.set(cacheKey, texture);
  return texture;
};

export const getEdgeColor = (): string => {
  const rootTheme = document.getElementById('root')?.getAttribute('theme');
  const theme = rootTheme ?? '';
  if (theme === 'Dark' || theme === 'Ocean') {
    return getComputedStyle(document.documentElement).getPropertyValue('--thorium-text-secondary');
  }
  return 'darkgray';
};
