import { useReducer } from 'react';
import { OriginState, DEFAULT_ORIGIN_STATE } from './types';

type OriginAction =
  | { type: 'SET_FIELD'; field: keyof OriginState; value: string }
  | { type: 'RESET' };

function originReducer(state: OriginState, action: OriginAction): OriginState {
  switch (action.type) {
    case 'SET_FIELD':
      return { ...state, [action.field]: action.value };
    case 'RESET':
      return DEFAULT_ORIGIN_STATE;
    default:
      return state;
  }
}

export function useOriginState() {
  const [originState, dispatch] = useReducer(originReducer, DEFAULT_ORIGIN_STATE);

  const setOriginField = (field: keyof OriginState, value: string) => {
    dispatch({ type: 'SET_FIELD', field, value });
  };

  const resetOriginState = () => {
    dispatch({ type: 'RESET' });
  };

  return { originState, setOriginField, resetOriginState };
}
