import { NavLink, Route, Routes } from 'react-router-dom';
import { hasToken } from './api';
import Chain from './pages/Chain';
import Config from './pages/Config';
import History from './pages/History';
import Run from './pages/Run';
import RunDetail from './pages/RunDetail';

export default function App() {
  return (
    <div className="layout">
      <nav className="sidebar">
        <div className="brand">aido</div>
        <NavLink to="/" end className={({ isActive }) => (isActive ? 'nav active' : 'nav')}>
          运行
        </NavLink>
        <NavLink to="/history" className={({ isActive }) => (isActive ? 'nav active' : 'nav')}>
          历史
        </NavLink>
        <NavLink to="/chain" className={({ isActive }) => (isActive ? 'nav active' : 'nav')}>
          链
        </NavLink>
        <NavLink to="/config" className={({ isActive }) => (isActive ? 'nav active' : 'nav')}>
          配置
        </NavLink>
        <div className="sidebar-foot">
          <a
            className="nav hint"
            href="https://github.com/appstore/aido"
            target="_blank"
            rel="noreferrer"
          >
            CLI 文档 ↗
          </a>
        </div>
      </nav>
      <main className="content">
        {!hasToken && (
          <div className="banner warn">
            缺少会话令牌：请从 <code>aido ui</code> 打印的链接进入（URL 里的 <code>?t=…</code>）。
          </div>
        )}
        <Routes>
          <Route path="/" element={<Run />} />
          <Route path="/chain" element={<Chain />} />
          <Route path="/history" element={<History />} />
          <Route path="/config" element={<Config />} />
          <Route path="/runs/:id" element={<RunDetail />} />
          <Route path="*" element={<div className="empty">没有这个页面。</div>} />
        </Routes>
      </main>
    </div>
  );
}
