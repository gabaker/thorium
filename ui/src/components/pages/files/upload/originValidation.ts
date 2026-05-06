import { OriginState, TagEntry } from './types';
import { hasInvalidTags } from '@utilities/tags';

type ValidationResult = { success: true } | { success: false; error: string };

export function buildUploadFormBase(
  description: string,
  selectedGroups: string[],
  tags: TagEntry[],
  tlpTags: TagEntry[],
): { form: FormData } | { errors: string[] } {
  const form = new FormData();

  if (description) {
    form.append('description', description);
  }

  if (selectedGroups.length === 0) {
    return { errors: ['At least one group must be selected to submit a file'] };
  }
  selectedGroups.forEach((group) => form.append('groups', group));

  if (tlpTags) {
    tlpTags.forEach((tag) => {
      if (tag.key && tag.value) {
        form.append(`tags[${tag.key}]`, tag.value);
      }
    });
  }

  if (tags) {
    if (hasInvalidTags(tags, true)) {
      return { errors: ['Invalid tags present. Please delete or fix before uploading'] };
    }
    tags.forEach((tag) => {
      if (tag.key && tag.value) {
        form.append(`tags[${tag.key}]`, tag.value);
      }
    });
  }

  return { form };
}

export function appendOriginToForm(formBase: FormData, origin: OriginState): ValidationResult {
  const { originType } = origin;

  if (originType === 'Downloaded') {
    return appendDownloadedOrigin(formBase, origin);
  } else if (originType === 'Transformed') {
    return appendTransformedOrigin(formBase, origin);
  } else if (originType === 'Unpacked') {
    return appendUnpackedOrigin(formBase, origin);
  } else if (originType === 'Carved') {
    return appendCarvedOrigin(formBase, origin);
  } else if (originType === 'Wire') {
    return appendWireOrigin(formBase, origin);
  } else if (originType === 'Incident') {
    return appendIncidentOrigin(formBase, origin);
  } else if (originType === 'MemoryDump') {
    return appendMemoryDumpOrigin(formBase, origin);
  }

  return { success: true };
}

function appendDownloadedOrigin(form: FormData, origin: OriginState): ValidationResult {
  if (origin.originUrl) {
    form.append('origin[origin_type]', 'Downloaded');
    form.append('origin[url]', origin.originUrl);
    if (origin.originName) {
      form.append('origin[name]', origin.originName);
    }
  } else if (origin.originName) {
    return { success: false, error: 'ORIGIN field "SITE NAME" set while necessary field "URL" is blank' };
  }
  return { success: true };
}

function appendTransformedOrigin(form: FormData, origin: OriginState): ValidationResult {
  if (origin.originParentFile) {
    form.append('origin[origin_type]', 'Transformed');
    form.append('origin[parent]', origin.originParentFile);
    if (origin.originTool) {
      form.append('origin[tool]', origin.originTool);
    }
    if (origin.originToolFlags) {
      form.append('origin[flags]', origin.originToolFlags);
    }
  } else if (origin.originTool) {
    return { success: false, error: 'ORIGIN field "TOOL" set while necessary field "PARENT" is blank' };
  } else if (origin.originToolFlags) {
    return { success: false, error: 'ORIGIN field "FLAGS" set while necessary field "PARENT" is blank' };
  }
  return { success: true };
}

function appendUnpackedOrigin(form: FormData, origin: OriginState): ValidationResult {
  if (origin.originParentFile) {
    form.append('origin[origin_type]', 'Unpacked');
    form.append('origin[parent]', origin.originParentFile);
    if (origin.originTool) {
      form.append('origin[tool]', origin.originTool);
    }
    if (origin.originToolFlags) {
      form.append('origin[flags]', origin.originToolFlags);
    }
  } else if (origin.originTool) {
    return { success: false, error: 'ORIGIN field "TOOL" set while necessary field "PARENT" is blank' };
  } else if (origin.originToolFlags) {
    return { success: false, error: 'ORIGIN field "FLAGS" set while necessary field "PARENT" is blank' };
  }
  return { success: true };
}

function appendCarvedOrigin(form: FormData, origin: OriginState): ValidationResult {
  if (origin.originParentFile) {
    if (!origin.carvedType) {
      return { success: false, error: 'ORIGIN "Carved" needs a specified type' };
    }
    form.append('origin[parent]', origin.originParentFile);
    if (origin.originTool) {
      form.append('origin[tool]', origin.originTool);
    }
    const totalType = 'Carved' + origin.carvedType;
    form.append('origin[origin_type]', totalType);
    if (totalType === 'CarvedPcap') {
      if (origin.originSourceIp) {
        form.append('origin[src_ip]', origin.originSourceIp);
      }
      if (origin.originDestinationIp) {
        form.append('origin[dest_ip]', origin.originDestinationIp);
      }
      if (origin.originSourcePort) {
        form.append('origin[src_port]', origin.originSourcePort);
      }
      if (origin.originDestinationPort) {
        form.append('origin[dest_port]', origin.originDestinationPort);
      }
      if (origin.originProtocol) {
        form.append('origin[proto]', origin.originProtocol);
      }
      if (origin.originCarvedPcapUrl) {
        form.append('origin[url]', origin.originCarvedPcapUrl);
      }
    }
  } else if (origin.originTool) {
    return { success: false, error: 'ORIGIN field "TOOL" set while necessary field "PARENT" is blank' };
  }
  return { success: true };
}

function appendWireOrigin(form: FormData, origin: OriginState): ValidationResult {
  if (origin.originSniffer) {
    form.append('origin[origin_type]', 'Wire');
    form.append('origin[sniffer]', origin.originSniffer);
    if (origin.originSource) {
      form.append('origin[source]', origin.originSource);
    }
    if (origin.originDestination) {
      form.append('origin[destination]', origin.originDestination);
    }
  } else if (origin.originSource) {
    return { success: false, error: 'ORIGIN field "SOURCE" set while necessary field "SNIFFER" is blank' };
  } else if (origin.originDestination) {
    return { success: false, error: 'ORIGIN field "DESTINATION" set while necessary field "SNIFFER" is blank' };
  }
  return { success: true };
}

function appendIncidentOrigin(form: FormData, origin: OriginState): ValidationResult {
  if (origin.originIncident) {
    form.append('origin[origin_type]', 'Incident');
    form.append('origin[incident]', origin.originIncident);
    if (origin.originMissionTeam) {
      form.append('origin[mission_team]', origin.originMissionTeam);
    }
    if (origin.originCoverTerm) {
      form.append('origin[cover_term]', origin.originCoverTerm);
    }
    if (origin.originNetwork) {
      form.append('origin[network]', origin.originNetwork);
    }
    if (origin.originMachine) {
      form.append('origin[machine]', origin.originMachine);
    }
    if (origin.originLocation) {
      form.append('origin[location]', origin.originLocation);
    }
  } else if (origin.originCoverTerm) {
    return { success: false, error: 'ORIGIN field "COVER TERM" set while necessary field "INCIDENT ID" is blank' };
  } else if (origin.originMissionTeam) {
    return { success: false, error: 'ORIGIN field "MISSION TEAM" set while necessary field "INCIDENT ID" is blank' };
  } else if (origin.originNetwork) {
    return { success: false, error: 'ORIGIN field "NETWORK" set while necessary field "INCIDENT ID" is blank' };
  } else if (origin.originMachine) {
    return { success: false, error: 'ORIGIN field "MACHINE" set while necessary field "INCIDENT ID" is blank' };
  } else if (origin.originLocation) {
    return { success: false, error: 'ORIGIN field "LOCATION" set while necessary field "INCIDENT ID" is blank' };
  }
  return { success: true };
}

function appendMemoryDumpOrigin(form: FormData, origin: OriginState): ValidationResult {
  if (origin.originMemoryType) {
    form.append('origin[origin_type]', 'MemoryDump');
    form.append('origin[memory_type]', origin.originMemoryType);
    if (origin.originParentFile) {
      form.append('origin[parent]', origin.originParentFile);
    }
    if (origin.originReconstructed) {
      form.append('origin[reconstructed]', origin.originReconstructed);
    }
    if (origin.originBaseAddress) {
      form.append('origin[base_addr]', origin.originBaseAddress);
    }
  } else if (origin.originParentFile) {
    return { success: false, error: 'ORIGIN field "PARENT" set while necessary field "MEMORY TYPE" is blank' };
  } else if (origin.originReconstructed.length > 0) {
    return { success: false, error: 'ORIGIN field "RECONSTRUCTED" set while necessary field "MEMORY TYPE" is blank' };
  } else if (origin.originBaseAddress) {
    return { success: false, error: 'ORIGIN field "BASE ADDRESS" set while necessary field "MEMORY TYPE" is blank' };
  }
  return { success: true };
}
