import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'

import { App } from './App'
import { AppStateProvider } from './state/store'
import './style.css'

const root = document.getElementById('root')
if (root === null) {
  throw new Error('找不到 #root 挂载点')
}

createRoot(root).render(
  <StrictMode>
    <AppStateProvider>
      <App />
    </AppStateProvider>
  </StrictMode>,
)
