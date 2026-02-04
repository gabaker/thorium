import React, { createContext, useContext, useReducer, Dispatch } from 'react';
import { uploadFormReducer, UploadFormState, UploadFormAction, initialUploadFormState } from './upload_reducer';

interface UploadFormContextType {
  state: UploadFormState;
  dispatch: Dispatch<UploadFormAction>;
}

const UploadFormContext = createContext<UploadFormContextType | undefined>(undefined);

export const UploadFormProvider: React.FC<{ entity: any; children: React.ReactNode }> = ({ entity, children }) => {
  const [state, dispatch] = useReducer(uploadFormReducer, initialUploadFormState(entity));
  return <UploadFormContext.Provider value={{ state, dispatch }}>{children}</UploadFormContext.Provider>;
};

export const useUploadForm = (): UploadFormContextType => {
  const context = useContext(UploadFormContext);
  if (context === undefined) {
    throw new Error('useUploadForm must be used within an UploadFormProvider');
  }
  return context;
};
