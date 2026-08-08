import { useState } from "react";
import styles from "./Counter.module.css";

interface CounterProps {
  label: string;
}

export function Counter({ label }: CounterProps) {
  const [count, setCount] = useState<number>(0);
  return (
    <button className={styles.button} onClick={() => setCount((n) => n + 1)}>
      {label}: {count}
    </button>
  );
}
