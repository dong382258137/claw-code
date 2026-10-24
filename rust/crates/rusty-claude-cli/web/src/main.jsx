import React from 'react'
import { createRoot } from 'react-dom/client'
import App, { ClientErrorBoundary } from './App.jsx'
import './styles.css'

createRoot(document.getElementById('root')).render(
  <ClientErrorBoundary>
    <App />
  </ClientErrorBoundary>,
)
