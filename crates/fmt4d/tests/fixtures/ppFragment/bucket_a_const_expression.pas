unit BucketAConstExpression;

interface

const
  REFRESH_INTERVAL_SHORT = {$IFDEF UNITTEST} 100 {$ELSE} 3000 {$ENDIF};

implementation

end.
