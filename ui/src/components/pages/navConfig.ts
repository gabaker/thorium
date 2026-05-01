import React from 'react';
import { IconType } from 'react-icons';
import {
  FaUpload,
  FaSearch,
  FaLayerGroup,
  FaFolderOpen,
  FaFolder,
  FaSitemap,
  FaCube,
  FaUsers,
  FaUser,
  FaCog,
  FaChartLine,
  FaTools,
  FaUserShield,
  FaCodeBranch,
  FaTachometerAlt,
  FaExclamationTriangle,
} from 'react-icons/fa';
import { FaHardDrive, FaFolderTree } from 'react-icons/fa6';
import { MdBusinessCenter } from 'react-icons/md';
import SigmaIcon from '@components/shared/icons/SigmaIcon';

export type NavIcon = IconType | React.ComponentType<{ size?: number }>;

export interface NavSubItem {
  label: string;
  icon: NavIcon;
  path: string;
}

export interface NavCategory {
  label: string;
  icon: NavIcon;
  path?: string;
  children?: NavSubItem[];
  adminOnly?: boolean;
}

export const NAV_ITEMS: NavCategory[] = [
  { label: 'Search', icon: FaSearch, path: '/' },
  { label: 'Analyze', icon: FaUpload, path: '/upload' },
  {
    label: 'Browse',
    icon: FaLayerGroup,
    children: [
      { label: 'Files', icon: FaFolderOpen, path: '/files' },
      { label: 'Repos', icon: FaCodeBranch, path: '/repos' },
      { label: 'Collections', icon: FaFolder, path: '/collections' },
      { label: 'Devices', icon: FaHardDrive, path: '/devices' },
      { label: 'Vendors', icon: MdBusinessCenter, path: '/vendors' },
      { label: 'File Systems', icon: FaFolderTree, path: '/filesystems' },
      { label: 'Sigma Rules', icon: SigmaIcon, path: '/sigma-rules' },
    ],
  },
  {
    label: 'Tools',
    icon: FaTools,
    children: [
      { label: 'Pipelines', icon: FaSitemap, path: '/pipelines' },
      { label: 'Images', icon: FaCube, path: '/images' },
      { label: 'Stats', icon: FaChartLine, path: '/stats' },
    ],
  },
  {
    label: 'Dashboards',
    icon: FaTachometerAlt,
    children: [{ label: 'Incident', icon: FaExclamationTriangle, path: '/dashboard/incident' }],
  },
  { label: 'Groups', icon: FaUsers, path: '/groups' },
  {
    label: 'Admin',
    icon: FaUserShield,
    adminOnly: true,
    children: [
      { label: 'Users', icon: FaUser, path: '/users' },
      { label: 'Settings', icon: FaCog, path: '/settings' },
    ],
  },
];
