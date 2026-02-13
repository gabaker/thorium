# Upload Component Refactoring - Verification Checklist

## Prerequisites

Ensure you have Node.js and npm installed and available in your PATH.

## Step 1: Format Code

Run the formatter to ensure consistent code style:

```bash
cd /Users/gavin/Projects/thorium-gabaker/ui
npm run format
```

Expected: All files formatted successfully with no errors.

## Step 2: Validate TypeScript Types

Check for TypeScript compilation errors:

```bash
cd /Users/gavin/Projects/thorium-gabaker/ui
npm run build
```

Expected: No TypeScript errors in upload component files.

## Step 3: Lint Check

Verify code follows linting rules:

```bash
cd /Users/gavin/Projects/thorium-gabaker/ui
npm run lint
```

Expected: No linting errors (or only pre-existing errors unrelated to upload component).

## Step 4: Manual Testing

### 4.1 Basic Upload Flow

1. Navigate to a page that uses the Upload component
2. Select a file using the dropzone
3. Select at least one group
4. Click Upload
5. Verify file uploads successfully

**Expected Results:**

- File selection works
- Groups can be selected
- Upload completes successfully
- Success message appears with SHA256 link

### 4.2 Form Validation

1. Try uploading without selecting a file
2. Try uploading without selecting a group
3. Try setting origin fields incorrectly (e.g., Site Name without URL)

**Expected Results:**

- Appropriate error messages appear
- Form validation prevents invalid submissions

### 4.3 Multiple File Upload

1. Select multiple files (3+)
2. Configure form fields
3. Click Upload
4. Observe upload status panel

**Expected Results:**

- Upload status panel appears
- Individual file progress shown
- All files upload successfully

### 4.4 TLP Selection

1. Click different TLP levels
2. Verify only one can be selected at a time
3. Upload a file with TLP selected

**Expected Results:**

- TLP buttons toggle correctly
- Only one TLP selected at a time
- TLP tag appears on uploaded file

### 4.5 Origin Forms

Test each origin type tab:

**Downloaded:**

1. Enter URL
2. Optionally enter Site Name
3. Upload

**Transformed/Unpacked:**

1. Enter Parent SHA256
2. Optionally enter Tool and Flags
3. Upload

**Carved:**

1. Select Pcap or Unknown
2. Enter Parent SHA256
3. For Pcap: optionally enter IP addresses, ports, protocol, URL
4. Upload

**Wire:**

1. Enter Sniffer name
2. Optionally enter Source and Destination
3. Upload

**Incident:**

1. Enter Incident ID
2. Optionally enter Cover Term, Mission Team, Network, Machine, Location
3. Upload

**Memory Dump:**

1. Enter Memory Type
2. Optionally enter Parent, Reconstructed list, Base Address
3. Upload

**Expected Results:**

- All tabs accessible
- Fields validate correctly
- Origin data persists through upload

### 4.6 Tags

1. Add multiple tag key-value pairs
2. Remove tags
3. Upload file with tags

**Expected Results:**

- Tags can be added/removed
- Tags appear on uploaded file

### 4.7 Pipelines

1. Select one or more pipelines
2. Upload file
3. Wait for reactions to complete

**Expected Results:**

- Pipelines can be selected
- Reactions submitted successfully
- Reaction results appear

### 4.8 Upload Status Panel (Multiple Files)

1. Upload 3+ files
2. Observe status panel
3. Click dropdown arrows to view reaction details

**Expected Results:**

- Overall progress bar shows total progress
- Individual file statuses shown
- Reaction results expandable
- Can navigate back to form

### 4.9 Retry Functionality

1. Simulate upload failure (disconnect network or use invalid form)
2. Click retry button on failed upload
3. Verify retry works

**Expected Results:**

- Failed uploads marked with error
- Retry button appears
- Retry successfully re-attempts upload

### 4.10 Cancel Upload

1. Start uploading large file(s)
2. Click Cancel button while in progress

**Expected Results:**

- Upload cancels
- Can start new upload

## Step 5: Code Review

Review the following aspects:

### Code Quality

- [ ] All functions have JSDoc comments
- [ ] Complex logic is explained with inline comments
- [ ] No console.log statements (except intentional logging)
- [ ] Consistent naming conventions

### Type Safety

- [ ] No TypeScript errors
- [ ] No `any` types except where necessary (@ts-nocheck)
- [ ] Reducer actions properly typed
- [ ] Context properly typed

### Performance

- [ ] No unnecessary re-renders
- [ ] State updates are batched where possible
- [ ] Context provider doesn't cause performance issues

### Accessibility

- [ ] Form fields have proper labels
- [ ] Error messages are clear
- [ ] Keyboard navigation works

## Step 6: Regression Testing

Compare with original functionality:

1. All features that worked before still work
2. No new bugs introduced
3. Performance is the same or better
4. User experience is unchanged or improved

## Common Issues & Solutions

### Issue: Context undefined error

**Solution:** Ensure UploadFormProvider wraps all components using useUploadForm

### Issue: State updates not reflecting

**Solution:** Check reducer is handling action type correctly

### Issue: Props not found error

**Solution:** Component may still be receiving props instead of using context

### Issue: Import errors

**Solution:** Check index.ts exports are correct

## Success Criteria

- [ ] All tests pass
- [ ] Code formatted with `npm run format`
- [ ] No TypeScript compilation errors
- [ ] No linting errors
- [ ] All manual tests pass
- [ ] Code is well-documented
- [ ] Performance is acceptable
- [ ] User experience is maintained

## Rollback Plan

If critical issues are found:

1. Revert to previous commit
2. Document specific issues
3. Create targeted fixes
4. Re-test before re-deploying

## Notes

Record any issues or observations during testing:

```
Date: ___________
Tester: ___________

Issues Found:
1.
2.
3.

Observations:
1.
2.
3.
```
