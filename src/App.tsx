import { useEffect, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import './App.css';

function App() {
  const [svgContent, setSvgContent] = useState('');

  useEffect(() => {
    const unlisten = listen('orderbook-update', (event: any) => {
      setSvgContent(event.payload.svg);
    });

    return () => {
      unlisten.then(f => f());
    };
  }, []);

  return (
    <div style={{
      width: '100vw',
      height: '100vh',
      display: 'flex',
      flexDirection: 'column',
      backgroundColor: '#000',
      margin: 0,
      padding: 0,
      overflow: 'hidden'
    }}>
      <div style={{
        flex: 1,
        width: '100%',
        height: '100%',
        display: 'flex',
        justifyContent: 'center',
        alignItems: 'center'
      }}
        dangerouslySetInnerHTML={{ __html: svgContent }}
      />
    </div>
  );
}

export default App;
