#import <Foundation/Foundation.h>
#import <Speech/Speech.h>
#import <dispatch/dispatch.h>
#include <stdlib.h>
#include <string.h>

typedef struct MjSpeechTranscriptionResult {
    int kind;
    char *text;
    char *message;
} MjSpeechTranscriptionResult;

static char *mj_strdup_string(NSString *value) {
    if (value == nil) {
        return NULL;
    }
    const char *utf8 = value.UTF8String;
    if (utf8 == NULL) {
        return strdup("");
    }
    return strdup(utf8);
}

static MjSpeechTranscriptionResult mj_result(int kind, NSString *text, NSString *message) {
    MjSpeechTranscriptionResult result;
    result.kind = kind;
    result.text = mj_strdup_string(text);
    result.message = mj_strdup_string(message);
    return result;
}

static NSString *mj_trimmed_string(NSString *value) {
    if (value == nil) {
        return nil;
    }
    NSString *trimmed = [value
        stringByTrimmingCharactersInSet:[NSCharacterSet whitespaceAndNewlineCharacterSet]];
    if (trimmed == nil || trimmed.length == 0) {
        return nil;
    }
    return trimmed;
}

MjSpeechTranscriptionResult mj_transcribe_wav_file(const char *path) {
    @autoreleasepool {
        if (path == NULL) {
            return mj_result(2, nil, @"Missing voice recording path");
        }

        if (@available(macOS 10.15, *)) {
            NSString *filePath = [NSString stringWithUTF8String:path];
            if (filePath == nil) {
                return mj_result(2, nil, @"Voice recording path was not valid UTF-8");
            }

            SFSpeechRecognizerAuthorizationStatus status =
                [SFSpeechRecognizer authorizationStatus];
            if (status == SFSpeechRecognizerAuthorizationStatusNotDetermined) {
                dispatch_semaphore_t authSemaphore = dispatch_semaphore_create(0);
                __block SFSpeechRecognizerAuthorizationStatus resolved = status;
                [SFSpeechRecognizer requestAuthorization:^(
                    SFSpeechRecognizerAuthorizationStatus authStatus
                ) {
                    resolved = authStatus;
                    dispatch_semaphore_signal(authSemaphore);
                }];
                dispatch_time_t authTimeout =
                    dispatch_time(DISPATCH_TIME_NOW, 10 * NSEC_PER_SEC);
                if (dispatch_semaphore_wait(authSemaphore, authTimeout) != 0) {
                    return mj_result(
                        1,
                        nil,
                        @"Timed out while waiting for speech recognition authorization"
                    );
                }
                status = resolved;
            }

            if (status != SFSpeechRecognizerAuthorizationStatusAuthorized) {
                return mj_result(
                    1,
                    nil,
                    @"Speech recognition authorization was not granted"
                );
            }

            SFSpeechRecognizer *recognizer = [[SFSpeechRecognizer alloc] init];
            if (recognizer == nil) {
                return mj_result(1, nil, @"Speech recognizer is unavailable");
            }
            if ([recognizer respondsToSelector:@selector(supportsOnDeviceRecognition)] &&
                !recognizer.supportsOnDeviceRecognition) {
                return mj_result(
                    1,
                    nil,
                    @"On-device speech transcription is unavailable for the current locale"
                );
            }

            NSURL *url = [NSURL fileURLWithPath:filePath];
            SFSpeechURLRecognitionRequest *request =
                [[SFSpeechURLRecognitionRequest alloc] initWithURL:url];
            if (request == nil) {
                return mj_result(2, nil, @"Failed to create speech recognition request");
            }
            if ([request respondsToSelector:@selector(setRequiresOnDeviceRecognition:)]) {
                request.requiresOnDeviceRecognition = YES;
            }
            request.shouldReportPartialResults = YES;

            dispatch_semaphore_t semaphore = dispatch_semaphore_create(0);
            __block NSString *bestText = nil;
            __block NSString *errorText = nil;
            SFSpeechRecognitionTask *task = [recognizer
                recognitionTaskWithRequest:request
                              resultHandler:^(SFSpeechRecognitionResult *_Nullable result,
                                              NSError *_Nullable error) {
                                  if (result != nil) {
                                      bestText = result.bestTranscription.formattedString;
                                      if (result.isFinal) {
                                          dispatch_semaphore_signal(semaphore);
                                      }
                                  }
                                  if (error != nil) {
                                      errorText = error.localizedDescription ?: @"Speech transcription failed";
                                      dispatch_semaphore_signal(semaphore);
                                  }
                              }];

            dispatch_time_t timeout =
                dispatch_time(DISPATCH_TIME_NOW, 60 * NSEC_PER_SEC);
            if (dispatch_semaphore_wait(semaphore, timeout) != 0) {
                [task cancel];
                NSString *trimmed = mj_trimmed_string(bestText);
                if (trimmed != nil) {
                    return mj_result(0, trimmed, nil);
                }
                return mj_result(1, nil, @"Local speech transcription did not finish in time");
            }

            [task cancel];
            if (errorText != nil) {
                return mj_result(2, nil, errorText);
            }

            NSString *trimmed = mj_trimmed_string(bestText);
            if (trimmed == nil) {
                return mj_result(1, nil, @"Local speech transcription returned no text");
            }

            return mj_result(0, trimmed, nil);
        }

        return mj_result(1, nil, @"Speech framework requires macOS 10.15 or newer");
    }
}

void mj_free_c_string(char *ptr) {
    free(ptr);
}
